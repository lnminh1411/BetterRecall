// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Core;

/// <summary>
/// Settings recommendations from a full benchmark (v1-design section 10). Pure: same report and settings, same output.
/// Each rule yields a Recommendation only when its value differs from the current one. Value and Current are
/// compact JSON literals.
/// </summary>
public static class Recommender
{
    public const string RDevice = "R_device_fastest";
    public const string RGpuAdapter = "R_gpu_fastest";
    public const string RTokens = "R_tokens_budget";
    public const string RInterval = "R_interval_slow_inference";
    public const string REncoder = "R_encoder_realtime";
    public const string RLayout = "R_layout";
    public const string RFps = "R_fps_headroom";
    public const string RResolution = "R_resolution_realtime";
    public const string RBitrate = "R_bitrate_quality";
    public const string RSeconds = "R_seconds_ram";
    public const string RUnsupported = "R_replay_unsupported";
    public const string RDim = "R_dim_ram";
    public const string RBattery = "R_battery";

    internal const double DutyLimit = 0.5;          // embed duty must stay under 50%
    internal const double RealtimeShare = 0.5;      // P95 encode under 50% of the frame period
    internal const double HighFpsShare = 0.25;      // high-refresh fps needs P95 under 25% of the period
    internal const double CpuLimitPercent = 1.0;    // agent CPU, percent of one core, static screen
    internal const long LowRamMb = 8 * 1024;

    private static readonly int[] TokenChoices = [280, 140, 70];
    private static readonly int[] IntervalChoices = [2000, 3000, 5000, 10000];
    private static readonly int[] BaseFps = [60, 30, 24, 15];
    private static readonly string[] Codecs = ["av1", "hevc", "h264"];
    private static readonly (string Name, int Height)[] LowerResolutions = [("1440p", 1440), ("1080p", 1080), ("720p", 720)];

    public static Recommendation[] Recommend(BenchReport report, Settings settings)
    {
        ArgumentNullException.ThrowIfNull(report);
        ArgumentNullException.ThrowIfNull(settings);
        var list = new List<Recommendation>();
        var hw = report.Hardware;

        InferenceRules(report, settings, list);
        ReplayRules(report, settings, list);

        if (hw.RamMb > 0 && hw.RamMb < LowRamMb)
            Add(list, "replay.seconds", 60, settings.Replay.Seconds, RSeconds, Mb(hw.RamMb));
        if (hw.RamMb > 0 && hw.RamMb < LowRamMb && settings.Retention.Days > 30)
            Add(list, "index.dim", 512, settings.Index.Dim, RDim, Mb(hw.RamMb), Inv(settings.Retention.Days));
        if (hw.HasBattery)
            Add(list, "governance.pauseOnBattery", true, settings.Governance.PauseOnBattery, RBattery);
        return [.. list];
    }

    // ------------------------------------------------------------ inference

    /// <summary>Keyframes per second the embedder must absorb: monitors x 0.3 / interval seconds.</summary>
    public static double ExpectedKeyframesPerSecond(int monitors, int intervalMs) => Math.Max(1, monitors) * 0.3 / (intervalMs / 1000.0);

    /// <summary>Embed duty cycle (0..1) for one image costing imageMs at the given keyframe rate.</summary>
    public static double Duty(double imageMs, double keyframesPerSecond) => imageMs / 1000.0 * keyframesPerSecond;

    internal static int CapturedMonitors(HardwareInfo hw, Settings s)
    {
        int all = Math.Max(1, hw.Monitors?.Length ?? 1);
        if (s.Capture.Monitors is not { Length: > 0 } chosen) return all;
        int n = hw.Monitors is { Length: > 0 } ms ? chosen.Distinct().Count(i => ms.Any(m => m.Index == i)) : chosen.Distinct().Count();
        return Math.Max(1, n);
    }

    private static void InferenceRules(BenchReport r, Settings s, List<Recommendation> list)
    {
        var inf = r.Inference;
        if (inf == null) return;
        var passing = inf.Devices?.Where(d => d.Ok && d.ImageMs > 0).ToList() ?? [];
        BenchDevice? fastest = passing.OrderBy(d => d.ImageMs).FirstOrDefault();
        string chosen = fastest?.Device ?? inf.Chosen;
        if (string.IsNullOrWhiteSpace(chosen) || chosen.Equals("none", StringComparison.OrdinalIgnoreCase)) return;

        // Auto policy (section 4): NPU and CPU; a GPU only when no NPU exists, and then the GPU that
        // inference.gpuAdapter "auto" picks (rows carry Adapter only on PCs with more than one GPU). Fastest passing among those.
        bool hasNpu = r.Hardware.Npu != null || passing.Any(d => Is(d.Device, "NPU")) || (inf.Devices?.Any(d => Is(d.Device, "NPU")) ?? false);
        var autoPick = passing.Where(d => Is(d.Device, "NPU") || Is(d.Device, "CPU") || (!hasNpu && Is(d.Device, "GPU") && (d.Adapter == null || d.AdapterAuto)))
            .OrderBy(d => d.ImageMs).FirstOrDefault();
        bool isAuto = autoPick != null && Is(autoPick.Device, chosen) && string.Equals(autoPick.Adapter, fastest?.Adapter, StringComparison.OrdinalIgnoreCase);
        string device = isAuto ? "auto" : chosen.ToLowerInvariant();
        string shown = fastest?.Adapter is { } adapterName ? $"{chosen.ToUpperInvariant()} ({adapterName})" : chosen.ToUpperInvariant();
        Add(list, "inference.device", device, s.Inference.Device, RDevice, shown, Ms(fastest?.ImageMs ?? 0));
        // A specific GPU: inference.gpuAdapter is its name, or auto when it is the GPU auto picks. A current value that
        // already names this GPU (part of its name) stays.
        if (fastest is { Adapter: { Length: > 0 } name } && Is(fastest.Device, "GPU"))
        {
            string cur = s.Inference.GpuAdapter ?? "auto";
            bool curAuto = string.IsNullOrWhiteSpace(cur) || Is(cur.Trim(), "auto");
            bool same = fastest.AdapterAuto ? curAuto : !curAuto && name.Contains(cur.Trim(), StringComparison.OrdinalIgnoreCase);
            if (!same) Add(list, "inference.gpuAdapter", fastest.AdapterAuto ? "auto" : name, cur, RGpuAdapter, name, Ms(fastest.ImageMs));
        }

        // Token budgets are timed on the worker's chosen device only (for a GPU, ChosenAdapter names which one).
        var tokens = inf.Tokens?.Where(t => Is(t.Device, chosen) && t.ImageMs > 0
            && (fastest?.Adapter == null || string.Equals(fastest.Adapter, inf.ChosenAdapter, StringComparison.OrdinalIgnoreCase))).ToList() ?? [];
        if (tokens.Count == 0) return;
        int monitors = CapturedMonitors(r.Hardware, s);
        double kps = ExpectedKeyframesPerSecond(monitors, s.Capture.IntervalMs);
        foreach (var t in TokenChoices)
        {
            var tb = tokens.FirstOrDefault(x => x.ImageTokens == t);
            if (tb == null) continue;
            double duty = Duty(tb.ImageMs, kps);
            if (duty <= DutyLimit)
            {
                Add(list, "inference.imageTokens", t, s.Inference.ImageTokens, RTokens, Inv(t), Ms(tb.ImageMs), Num(kps), Pct(duty));
                return;
            }
        }
        // Even the smallest budget breaks the duty rule: 70 tokens and a longer interval.
        var t70 = tokens.FirstOrDefault(x => x.ImageTokens == 70) ?? tokens.OrderBy(x => x.ImageMs).First();
        Add(list, "inference.imageTokens", 70, s.Inference.ImageTokens, RTokens, "70", Ms(t70.ImageMs), Num(kps), Pct(Duty(t70.ImageMs, kps)));
        int interval = IntervalChoices.FirstOrDefault(i => Duty(t70.ImageMs, ExpectedKeyframesPerSecond(monitors, i)) <= DutyLimit, IntervalChoices[^1]);
        var k = ExpectedKeyframesPerSecond(monitors, interval);
        Add(list, "capture.intervalMs", interval, s.Capture.IntervalMs, RInterval, Inv(interval), Ms(t70.ImageMs), Num(k), Pct(Duty(t70.ImageMs, k)));
    }

    // ------------------------------------------------------------ replay

    /// <summary>Frame period in ms.</summary>
    public static double PeriodMs(int fps) => 1000.0 / fps;

    /// <summary>Realtime rule: P95 encode time under share x frame period.</summary>
    public static bool Realtime(double p95Ms, int fps, double share = RealtimeShare) => p95Ms > 0 && p95Ms < share * PeriodMs(fps);

    /// <summary>pixels x fps x bpp / 1000, clamped to 500..50000, rounded to 500.</summary>
    public static int BitrateKbps(int width, int height, int fps, string codec)
    {
        double bpp = codec switch { "av1" => 0.035, "hevc" => 0.05, _ => 0.08 };
        double kbps = (double)width * height * fps * bpp / 1000.0;
        int rounded = (int)(Math.Round(kbps / 500.0, MidpointRounding.AwayFromZero) * 500);
        return Math.Clamp(rounded, 500, 50000);
    }

    internal sealed record Probe(double P95, double Cpu, bool Measured);

    /// <summary>
    /// Encoder cost at a size and fps. Exact bench row when present (a failed row fails); otherwise estimated from the
    /// nearest measured row of the same adapter, codec and layout, scaled by pixel count (per-frame cost does not
    /// depend on fps). Null when nothing usable was measured.
    /// </summary>
    internal static Probe? Cost(EncoderBench[] rows, string adapter, string codec, string layout, int width, int height, int fps)
    {
        var same = rows.Where(e => e.Adapter == adapter && e.Codec == codec && e.Layout == layout).ToList();
        var exact = same.FirstOrDefault(e => e.Height == height && e.Width == width && e.Fps == fps);
        if (exact != null) return exact.Ok ? new Probe(exact.EncodeMsP95, exact.CpuPercent, true) : null;
        var ok = same.Where(e => e.Ok && e.EncodeMsP95 > 0 && e.Width > 0 && e.Height > 0).ToList();
        if (ok.Count == 0) return null;
        // A size that failed at any fps is not estimated as passing.
        if (same.Any(e => !e.Ok && e.Height == height)) return null;
        double target = (double)width * height;
        var near = ok.OrderBy(e => Math.Abs(Math.Log((double)e.Width * e.Height / target))).ThenBy(e => Math.Abs(e.Fps - fps)).First();
        double scale = target / ((double)near.Width * near.Height);
        double cpu = near.CpuPercent * scale * fps / Math.Max(1, near.Fps);
        return new Probe(near.EncodeMsP95 * scale, cpu, false);
    }

    internal static (int Width, int Height)? Native(EncoderBench[] rows, string layout)
    {
        var e = rows.Where(x => x.Layout == layout && x.Width > 0 && x.Height > 0).OrderByDescending(x => (long)x.Width * x.Height).FirstOrDefault();
        return e == null ? null : (e.Width, e.Height);
    }

    /// <summary>Encoder adapters in preference order: integrated first, then the GPU driving the fewest monitors.</summary>
    internal static List<string> AdapterOrder(BenchReport r)
    {
        var names = r.Encoders.Select(e => e.Adapter).Distinct().ToList();
        var gpus = r.Hardware.Gpus ?? [];
        var mons = r.Hardware.Monitors ?? [];
        return names
            .OrderByDescending(n => gpus.FirstOrDefault(g => g.Name == n)?.Integrated ?? false)
            .ThenBy(n => mons.Count(m => m.Adapter == n))
            .ThenBy(n => n, StringComparer.Ordinal)
            .ToList();
    }

    private static (int W, int H) Scaled((int W, int H) native, int height)
        => height >= native.H ? native : ((int)Math.Round((double)native.W * height / native.H / 2) * 2, height);

    private static void ReplayRules(BenchReport r, Settings s, List<Recommendation> list)
    {
        var rows = r.Encoders ?? [];
        if (rows.Length == 0)
        {
            // The agent benchmarks every usable adapter x codec; no rows means no hardware encoder at all.
            Add(list, "replay.enabled", false, s.Replay.Enabled, RUnsupported, "720p", "15");
            return;
        }
        var adapters = AdapterOrder(r);
        int maxRefresh = r.Hardware.Monitors is { Length: > 0 } ms ? ms.Max(m => m.RefreshHz) : 60;
        if (maxRefresh <= 0) maxRefresh = 60;

        bool Passes(string adapter, string codec, string layout, (int W, int H) size, int fps, double share = RealtimeShare)
            => Cost(rows, adapter, codec, layout, size.W, size.H, fps) is { } p && Realtime(p.P95, fps, share);

        // replay.enabled: no adapter passes at 720p 15 fps in any layout or codec.
        bool any720 = false;
        foreach (var layout in new[] { "composited", "perMonitor" })
        {
            if (Native(rows, layout) is not { } nat) continue;
            var size = Scaled(nat, 720);
            if (adapters.Any(a => Codecs.Any(c => Passes(a, c, layout, size, 15)))) { any720 = true; break; }
        }
        if (!any720)
        {
            Add(list, "replay.enabled", false, s.Replay.Enabled, RUnsupported, "720p", "15");
            return;
        }

        // Layout and resolution, in preference order, at a 30 fps baseline (15 when nothing holds 30).
        var steps = new List<(string Layout, string Res, int Height)>();
        steps.Add(("composited", "native", int.MaxValue));
        steps.Add(("perMonitor", "native", int.MaxValue));
        foreach (var (name, h) in LowerResolutions) steps.Add(("composited", name, h));

        foreach (int baseFps in new[] { 30, 15 })
        {
            foreach (var (layout, res, h) in steps)
            {
                if (Native(rows, layout) is not { } nat) continue;
                if (res != "native" && h >= nat.Height) continue; // never upscale, native already covers it
                var size = res == "native" ? nat : Scaled(nat, h);
                foreach (var adapter in adapters)
                {
                    var codec = Codecs.FirstOrDefault(c => Passes(adapter, c, layout, size, baseFps));
                    if (codec == null) continue;
                    EmitReplay(list, s, rows, adapter, codec, layout, res, size, maxRefresh);
                    return;
                }
            }
        }
    }

    private static void EmitReplay(List<Recommendation> list, Settings s, EncoderBench[] rows, string adapter, string codec,
        string layout, string res, (int W, int H) size, int maxRefresh)
    {
        // fps: refresh rate (>= 120) with 25% headroom first, then 60, 30, 24, 15; capped at the monitor refresh rate.
        var candidates = new List<(int Fps, double Share)>();
        if (maxRefresh >= 120)
        {
            int hi = ReplaySettings.FpsChoices.Where(f => f <= maxRefresh).DefaultIfEmpty(0).Max();
            if (hi >= 120) candidates.Add((hi, HighFpsShare));
        }
        foreach (var f in BaseFps) if (f <= maxRefresh) candidates.Add((f, RealtimeShare));
        if (candidates.Count == 0) candidates.Add((15, RealtimeShare));

        (int Fps, Probe P)? pick = null, fallback = null;
        foreach (var (fps, share) in candidates)
        {
            if (Cost(rows, adapter, codec, layout, size.W, size.H, fps) is not { } p || !Realtime(p.P95, fps, share)) continue;
            fallback = (fps, p); // keeps the lowest realtime fps when none meets the CPU rule
            if (p.Cpu < CpuLimitPercent) { pick = (fps, p); break; }
        }
        var chosen = pick ?? fallback;
        if (chosen == null) return;
        var (bestFps, probe) = chosen.Value;

        string period = Ms(PeriodMs(bestFps));
        Add(list, "replay.encoder", codec, s.Replay.Encoder, REncoder, codec, adapter, Ms(probe.P95), period);
        Add(list, "replay.encoderAdapter", adapter, s.Replay.EncoderAdapter, REncoder, codec, adapter, Ms(probe.P95), period);
        Add(list, "replay.layout", layout, s.Replay.Layout, RLayout, layout, res);
        Add(list, "replay.resolution", res, s.Replay.Resolution, RResolution, res, Inv(size.W), Inv(size.H), Ms(probe.P95));
        Add(list, "replay.fps", bestFps, s.Replay.Fps, RFps, Inv(bestFps), Ms(probe.P95), period, Num(probe.Cpu));
        string bppText = codec switch { "av1" => "0.035", "hevc" => "0.05", _ => "0.08" };
        int kbps = BitrateKbps(size.W, size.H, bestFps, codec);
        Add(list, "replay.bitrateKbps", kbps, s.Replay.BitrateKbps, RBitrate, Inv(kbps), Inv(size.W), Inv(size.H), Inv(bestFps), bppText);
    }

    // ------------------------------------------------------------ helpers

    private static bool Is(string? a, string b) => string.Equals(a, b, StringComparison.OrdinalIgnoreCase);
    private static string Inv(int v) => v.ToString(CultureInfo.InvariantCulture);
    private static string Ms(double v) => v.ToString("0.#", CultureInfo.InvariantCulture);
    private static string Num(double v) => v.ToString("0.###", CultureInfo.InvariantCulture);
    private static string Pct(double v) => (v * 100).ToString("0.#", CultureInfo.InvariantCulture);
    private static string Mb(long mb) => mb.ToString(CultureInfo.InvariantCulture);

    private static void Add(List<Recommendation> list, string key, int value, int current, string reason, params string[] args)
    {
        if (value != current) list.Add(new Recommendation(key, Inv(value), Inv(current), reason, args));
    }

    private static void Add(List<Recommendation> list, string key, bool value, bool current, string reason, params string[] args)
    {
        if (value != current) list.Add(new Recommendation(key, value ? "true" : "false", current ? "true" : "false", reason, args));
    }

    private static void Add(List<Recommendation> list, string key, string value, string current, string reason, params string[] args)
    {
        if (!string.Equals(value, current, StringComparison.Ordinal))
            list.Add(new Recommendation(key, Json(value), Json(current ?? ""), reason, args));
    }

    private static string Json(string s) => JsonSerializer.Serialize(s, ContractsJson.Default.String);
}
