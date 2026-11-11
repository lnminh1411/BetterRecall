// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Core.Tests;

/// <summary>Fixed benchmark reports for hardware profiles (v1-design section 10).</summary>
internal static class Profiles
{
    private static EncoderBench E(string adapter, string codec, string layout, int w, int h, int fps, double p95, double cpu = 0.5, bool ok = true)
        => new(adapter, codec, codec == "av1" && adapter.StartsWith("Intel") ? "vpl" : "mf", layout, w, h, fps, ok, p95, cpu, 90, ok ? null : "failed");

    private static BenchReport Report(HardwareInfo hw, BenchResult? inf, EncoderBench[] enc)
        => new(DateTimeOffset.Parse("2026-11-10T08:00:00Z"), 240_000, hw, inf, enc, new CaptureBench(1.2, 6.5, 2.1), null, []);

    public const string IntelIgpu = "Intel(R) Graphics";

    /// <summary>Core Ultra 7 265K: NPU 3720, Arc iGPU (no monitor), RX 6600 + RTX 3050 driving 3 monitors, 32 GB.</summary>
    public static BenchReport ThisPc() => Report(
        new HardwareInfo("Intel(R) Core(TM) Ultra 7 265K", 20, 20, 32768, false,
            [
                new GpuInfo(IntelIgpu, "Intel", true, 0, "0x1", "32.0.101.6", ["av1", "hevc", "h264"]),
                new GpuInfo("AMD Radeon RX 6600", "AMD", false, 8192, "0x2", "32.0.21", ["hevc", "h264"]),
                new GpuInfo("NVIDIA GeForce RTX 3050", "NVIDIA", false, 8192, "0x3", "581.42", ["hevc", "h264"]),
            ],
            new NpuInfo("Intel(R) AI Boost", "32.0.100.4"),
            [
                new MonitorInfo(0, 2560, 1440, 165, "AMD Radeon RX 6600"),
                new MonitorInfo(1, 1920, 1080, 60, "AMD Radeon RX 6600"),
                new MonitorInfo(2, 1920, 1080, 60, "NVIDIA GeForce RTX 3050"),
            ], "Windows 11 29704"),
        new BenchResult("NPU",
            [new BenchDevice("NPU", "OpenVINO", true, 90, 12, 0.9995, null), new BenchDevice("GPU", "OpenVINO", true, 110, 8, 0.9996, null), new BenchDevice("CPU", "OpenVINO", true, 400, 30, 1, null)],
            [new TokenBench("NPU", 280, 650), new TokenBench("NPU", 140, 330), new TokenBench("NPU", 70, 170), new TokenBench("CPU", 140, 900)]),
        [
            E(IntelIgpu, "av1", "composited", 6400, 1440, 30, 9.0, 0.6),
            E(IntelIgpu, "av1", "composited", 6400, 1440, 60, 9.5, 0.9),
            E(IntelIgpu, "av1", "composited", 4800, 1080, 30, 5.0, 0.4),
            E(IntelIgpu, "av1", "composited", 4800, 1080, 60, 5.5, 0.7),
            E(IntelIgpu, "av1", "perMonitor", 2560, 1440, 30, 7.0, 0.9),
            E(IntelIgpu, "hevc", "composited", 6400, 1440, 30, 8.0, 0.6),
            E("AMD Radeon RX 6600", "hevc", "composited", 6400, 1440, 30, 4.0, 0.5),
            E("NVIDIA GeForce RTX 3050", "h264", "composited", 6400, 1440, 30, 3.0, 0.5),
        ]);

    public const string Rtx = "NVIDIA GeForce RTX 3050";
    public const string ArcOv = "Intel(R) Arc(TM) Graphics (iGPU)";

    /// <summary>This PC with the rows of a multi-GPU worker bench: every GPU measured, the RTX 3050 on DirectML fastest,
    /// the NPU chosen by the worker (auto policy), token budgets timed on the NPU.</summary>
    public static BenchReport MultiGpu() => ThisPc() with
    {
        Inference = new BenchResult("NPU",
            [
                new BenchDevice("NPU", "OpenVINO", true, 330, 12, 0.9995, null),
                new BenchDevice("CPU", "OpenVINO", true, 900, 30, 1, null),
                new BenchDevice("GPU", "OpenVINO", true, 500, 8, 0.9996, null, Adapter: ArcOv, AdapterAuto: true),
                new BenchDevice("GPU", "DirectML", true, 80, 6, 0.9997, null, Adapter: Rtx),
                new BenchDevice("GPU", "DirectML", false, 0, 0, 0, "cosine", Adapter: "AMD Radeon RX 6600"),
            ],
            [new TokenBench("NPU", 280, 650), new TokenBench("NPU", 140, 330), new TokenBench("NPU", 70, 170)]),
    };

    /// <summary>NVIDIA-only desktop, no NPU, one 4K 144 Hz monitor.</summary>
    public static BenchReport NvidiaDesktop() => Report(
        new HardwareInfo("AMD Ryzen 7 7800X3D", 8, 16, 65536, false,
            [new GpuInfo("NVIDIA GeForce RTX 4070", "NVIDIA", false, 12288, "0x1", "581.42", ["av1", "hevc", "h264"])],
            null, [new MonitorInfo(0, 3840, 2160, 144, "NVIDIA GeForce RTX 4070")], "Windows 11"),
        new BenchResult("GPU",
            [new BenchDevice("GPU", "DirectML", true, 40, 5, 0.9998, null), new BenchDevice("CPU", "CPU", true, 300, 25, 1, null)],
            [new TokenBench("GPU", 280, 80), new TokenBench("GPU", 140, 45), new TokenBench("GPU", 70, 30)]),
        [
            E("NVIDIA GeForce RTX 4070", "av1", "composited", 3840, 2160, 30, 3.0, 0.4),
            E("NVIDIA GeForce RTX 4070", "av1", "composited", 3840, 2160, 60, 3.2, 0.6),
            E("NVIDIA GeForce RTX 4070", "av1", "composited", 3840, 2160, 144, 3.3, 0.9),
        ]);

    /// <summary>AMD laptop: battery, 8 GB (7.5 GB usable), Radeon 780M, XDNA NPU that fails the cosine check.</summary>
    public static BenchReport AmdLaptop() => Report(
        new HardwareInfo("AMD Ryzen 7 7840U", 8, 16, 7680, true,
            [new GpuInfo("AMD Radeon 780M Graphics", "AMD", true, 2048, "0x1", "32.0.12", ["av1", "hevc", "h264"])],
            new NpuInfo("AMD Ryzen AI", "1.2"), [new MonitorInfo(0, 2880, 1800, 120, "AMD Radeon 780M Graphics")], "Windows 11"),
        new BenchResult("GPU",
            [new BenchDevice("NPU", "VitisAI", false, 0, 0, 0.91, "cosine"), new BenchDevice("GPU", "DirectML", true, 150, 20, 0.9997, null), new BenchDevice("CPU", "CPU", true, 600, 40, 1, null)],
            [new TokenBench("GPU", 280, 420), new TokenBench("GPU", 140, 230), new TokenBench("GPU", 70, 140)]),
        [
            E("AMD Radeon 780M Graphics", "av1", "composited", 2880, 1800, 30, 12.0, 0.8),
            E("AMD Radeon 780M Graphics", "av1", "composited", 2880, 1800, 60, 13.0, 1.2),
            E("AMD Radeon 780M Graphics", "av1", "composited", 1728, 1080, 30, 6.0, 0.5),
            E("AMD Radeon 780M Graphics", "av1", "composited", 1728, 1080, 60, 7.0, 0.8),
        ]);

    /// <summary>Old Intel (i7-8700, UHD 630): no AV1, slow inference, two 1080p60 monitors.</summary>
    public static BenchReport OldIntel() => Report(
        new HardwareInfo("Intel(R) Core(TM) i7-8700", 6, 12, 16384, false,
            [new GpuInfo("Intel(R) UHD Graphics 630", "Intel", true, 0, "0x1", "31.0.101", ["hevc", "h264"])],
            null,
            [new MonitorInfo(0, 1920, 1080, 60, "Intel(R) UHD Graphics 630"), new MonitorInfo(1, 1920, 1080, 60, "Intel(R) UHD Graphics 630")], "Windows 11"),
        new BenchResult("GPU",
            [new BenchDevice("GPU", "OpenVINO", true, 2600, 90, 0.9993, null), new BenchDevice("CPU", "OpenVINO", true, 3100, 120, 1, null)],
            [new TokenBench("GPU", 280, 5000), new TokenBench("GPU", 140, 2600), new TokenBench("GPU", 70, 2000)]),
        [
            E("Intel(R) UHD Graphics 630", "hevc", "composited", 3840, 1080, 30, 18.0, 1.5),
            E("Intel(R) UHD Graphics 630", "hevc", "perMonitor", 1920, 1080, 30, 7.0, 0.7),
            E("Intel(R) UHD Graphics 630", "hevc", "perMonitor", 1920, 1080, 60, 7.5, 1.4),
            E("Intel(R) UHD Graphics 630", "h264", "perMonitor", 1920, 1080, 30, 5.0, 0.6),
        ]);

    /// <summary>VM-like machine with a basic display adapter: no hardware encoder at all.</summary>
    public static BenchReport NoEncoder() => Report(
        new HardwareInfo("Intel(R) Xeon(R)", 4, 4, 16384, false,
            [new GpuInfo("Microsoft Basic Display Adapter", "Microsoft", false, 0, "0x1", "10.0", [])],
            null, [new MonitorInfo(0, 1920, 1080, 60, "Microsoft Basic Display Adapter")], "Windows 11"),
        new BenchResult("CPU", [new BenchDevice("CPU", "CPU", true, 700, 50, 1, null)], [new TokenBench("CPU", 140, 700)]),
        []);

    /// <summary>Ultrawide on an iGPU: native fails, 1080p (estimated from the bench row) passes.</summary>
    public static BenchReport Ultrawide() => Report(
        new HardwareInfo("cpu", 8, 8, 16384, false, [new GpuInfo("iGPU", "Intel", true, 0, "0x1", "1", ["hevc"])], null,
            [new MonitorInfo(0, 5120, 1440, 60, "iGPU")], "Windows 11"),
        null,
        [
            E("iGPU", "hevc", "composited", 5120, 1440, 30, 20.0, 0.9),
            E("iGPU", "hevc", "composited", 3840, 1080, 30, 8.0, 0.6),
        ]);
}

public class RecommenderTests
{
    private static Dictionary<string, Recommendation> Run(BenchReport r, Settings? s = null)
        => Recommender.Recommend(r, s ?? new Settings()).ToDictionary(x => x.Key);

    [Fact]
    public void Duty_cycle_maths()
    {
        Assert.Equal(0.45, Recommender.ExpectedKeyframesPerSecond(3, 2000), 6);
        Assert.Equal(0.15, Recommender.ExpectedKeyframesPerSecond(1, 2000), 6);
        Assert.Equal(0.06, Recommender.ExpectedKeyframesPerSecond(2, 10000), 6);
        Assert.Equal(0.2925, Recommender.Duty(650, 0.45), 6);
        Assert.True(Recommender.Duty(1111, 0.45) <= 0.5);
        Assert.False(Recommender.Duty(1112, 0.45) <= 0.5);
    }

    [Fact]
    public void Realtime_and_bitrate_maths()
    {
        Assert.Equal(1000.0 / 60, Recommender.PeriodMs(60), 9);
        Assert.True(Recommender.Realtime(8.33, 60));
        Assert.False(Recommender.Realtime(8.34, 60));
        Assert.True(Recommender.Realtime(16.6, 30));
        Assert.False(Recommender.Realtime(1.75, 144, 0.25));
        Assert.True(Recommender.Realtime(1.73, 144, 0.25));
        Assert.False(Recommender.Realtime(0, 30)); // no measurement never passes
        Assert.Equal(5000, Recommender.BitrateKbps(1920, 1080, 30, "h264"));   // 4977 -> 5000
        Assert.Equal(3000, Recommender.BitrateKbps(1920, 1080, 30, "hevc"));   // 3110 -> 3000
        Assert.Equal(17500, Recommender.BitrateKbps(3840, 2160, 60, "av1"));   // 17418 -> 17500
        Assert.Equal(500, Recommender.BitrateKbps(640, 360, 10, "av1"));       // clamp low
        Assert.Equal(50000, Recommender.BitrateKbps(7680, 4320, 240, "h264")); // clamp high
    }

    [Fact]
    public void Cost_estimate_scales_by_pixels_and_failed_rows_fail()
    {
        EncoderBench[] rows =
        [
            new("A", "hevc", "mf", "composited", 3840, 2160, 30, true, 8, 1.0, 50, null),
            new("A", "hevc", "mf", "composited", 1920, 1080, 60, false, 0, 0, 50, "x"),
        ];
        var est = Recommender.Cost(rows, "A", "hevc", "composited", 2560, 1440, 30)!;
        Assert.False(est.Measured);
        Assert.Equal(8 * (2560.0 * 1440) / (3840.0 * 2160), est.P95, 6);
        Assert.Null(Recommender.Cost(rows, "A", "hevc", "composited", 1920, 1080, 60)); // measured failure
        Assert.Null(Recommender.Cost(rows, "A", "av1", "composited", 3840, 2160, 30));  // codec not measured
        Assert.True(Recommender.Cost(rows, "A", "hevc", "composited", 3840, 2160, 30)!.Measured);
    }

    [Fact]
    public void This_pc()
    {
        var r = Run(Profiles.ThisPc());
        Assert.False(r.ContainsKey("inference.device"));          // NPU fastest = auto policy
        Assert.Equal("280", r["inference.imageTokens"].Value);     // 650 ms x 0.45/s = 29%
        Assert.Equal("140", r["inference.imageTokens"].Current);
        Assert.Equal(Recommender.RTokens, r["inference.imageTokens"].Reason);
        Assert.Equal(["280", "650", "0.45", "29.3"], r["inference.imageTokens"].Args);
        Assert.False(r.ContainsKey("capture.intervalMs"));
        Assert.Equal("\"av1\"", r["replay.encoder"].Value);
        Assert.Equal("\"auto\"", r["replay.encoder"].Current);
        Assert.Equal(Recommender.REncoder, r["replay.encoder"].Reason);
        Assert.Equal("\"Intel(R) Graphics\"", r["replay.encoderAdapter"].Value); // integrated, not driving a monitor
        Assert.False(r.ContainsKey("replay.layout"));             // composited passes at native
        Assert.False(r.ContainsKey("replay.resolution"));         // native
        Assert.False(r.ContainsKey("replay.fps"));                // 60 breaks 50% (9.5 > 8.33), 165 breaks 25%: 30 = current
        Assert.Equal("9500", r["replay.bitrateKbps"].Value);      // 6400x1440x30x0.035 = 9677
        Assert.Equal(Recommender.RBitrate, r["replay.bitrateKbps"].Reason);
        Assert.False(r.ContainsKey("replay.seconds"));
        Assert.False(r.ContainsKey("index.dim"));
        Assert.False(r.ContainsKey("governance.pauseOnBattery"));
        Assert.False(r.ContainsKey("replay.enabled"));
    }

    [Fact]
    public void Multi_gpu_recommends_the_fastest_graphics_card_by_name()
    {
        var r = Run(Profiles.MultiGpu());
        Assert.Equal("\"gpu\"", r["inference.device"].Value);
        Assert.Equal(["GPU (NVIDIA GeForce RTX 3050)", "80"], r["inference.device"].Args);
        Assert.Equal("\"NVIDIA GeForce RTX 3050\"", r["inference.gpuAdapter"].Value);
        Assert.Equal("\"auto\"", r["inference.gpuAdapter"].Current);
        Assert.Equal(Recommender.RGpuAdapter, r["inference.gpuAdapter"].Reason);
        Assert.Equal([Profiles.Rtx, "80"], r["inference.gpuAdapter"].Args);
        Assert.False(r.ContainsKey("inference.imageTokens"));      // budgets were timed on the NPU, not on this GPU

        // Applying both keys passes validation and leaves nothing to recommend.
        var s = new Settings();
        Apply(s, r["inference.device"]);
        Apply(s, r["inference.gpuAdapter"]);
        Assert.Equal(("gpu", Profiles.Rtx), (s.Inference.Device, s.Inference.GpuAdapter));
        Assert.DoesNotContain(Recommender.Recommend(Profiles.MultiGpu(), s), x => x.Key.StartsWith("inference.", StringComparison.Ordinal));

        // A current value that already names this card (part of its name) stays.
        s.Inference.GpuAdapter = "rtx 3050";
        Assert.DoesNotContain(Recommender.Recommend(Profiles.MultiGpu(), s), x => x.Key == "inference.gpuAdapter");
    }

    [Fact]
    public void Multi_gpu_auto_pick_sets_adapter_auto_and_tokens_follow_the_chosen_adapter()
    {
        var rep = Profiles.MultiGpu();
        var rows = rep.Inference!.Devices.Select(d => d.Adapter == Profiles.ArcOv ? d with { ImageMs = 60 } : d).ToArray();
        // The worker chose the iGPU (inference.device = gpu) and timed the budgets there.
        rep = rep with { Inference = rep.Inference with { Devices = rows, Chosen = "GPU", ChosenAdapter = Profiles.ArcOv,
            Tokens = [new TokenBench("GPU", 280, 120), new TokenBench("GPU", 140, 60), new TokenBench("GPU", 70, 40)] } };
        var s = new Settings();
        s.Inference.GpuAdapter = "RTX 3050";
        var r = Run(rep, s);
        Assert.Equal("\"gpu\"", r["inference.device"].Value);        // NPU exists: a GPU is never the auto policy
        Assert.Equal("\"auto\"", r["inference.gpuAdapter"].Value);   // the iGPU is what auto picks
        Assert.Equal("280", r["inference.imageTokens"].Value);         // tokens of the chosen adapter are used

        // No NPU: the auto GPU is the auto policy, so inference.device stays auto and the adapter goes back to auto.
        var noNpu = rep with { Hardware = rep.Hardware with { Npu = null }, Inference = rep.Inference! with { Devices = rows.Where(d => d.Device != "NPU").ToArray() } };
        r = Run(noNpu, s);
        Assert.False(r.ContainsKey("inference.device"));
        Assert.Equal("\"auto\"", r["inference.gpuAdapter"].Value);

        // Fastest is the RTX but the budgets were timed on the iGPU: no token recommendation from them.
        var rtxFastest = rep with { Inference = rep.Inference! with { Devices = rep.Inference.Devices.Select(d => d.Adapter == Profiles.ArcOv ? d with { ImageMs = 500 } : d).ToArray() } };
        Assert.False(Run(rtxFastest).ContainsKey("inference.imageTokens"));
    }

    [Fact]
    public void Nvidia_desktop_without_npu()
    {
        var r = Run(Profiles.NvidiaDesktop());
        Assert.False(r.ContainsKey("inference.device"));          // no NPU: GPU is in the auto policy and fastest
        Assert.Equal("280", r["inference.imageTokens"].Value);
        Assert.Equal("\"NVIDIA GeForce RTX 4070\"", r["replay.encoderAdapter"].Value);
        Assert.Equal("\"av1\"", r["replay.encoder"].Value);
        Assert.Equal("60", r["replay.fps"].Value);                // 144 needs < 1.74 ms (25%), 60 passes (3.2 < 8.33)
        Assert.Equal(Recommender.RFps, r["replay.fps"].Reason);
        Assert.Equal("17500", r["replay.bitrateKbps"].Value);
    }

    [Fact]
    public void Nvidia_desktop_high_refresh_with_headroom()
    {
        var rep = Profiles.NvidiaDesktop();
        rep = rep with { Encoders = rep.Encoders.Select(e => e.Fps == 144 ? e with { EncodeMsP95 = 1.5 } : e).ToArray() };
        var r = Run(rep);
        Assert.Equal("144", r["replay.fps"].Value);               // 1.5 ms < 25% of 6.94 ms
    }

    [Fact]
    public void Amd_laptop_battery_low_ram()
    {
        var s = new Settings();
        s.Retention.Days = 90;
        s.Governance.PauseOnBattery = false;
        var r = Run(Profiles.AmdLaptop(), s);
        Assert.Equal("\"gpu\"", r["inference.device"].Value);     // NPU exists, so auto would pick CPU; GPU is fastest
        Assert.Equal(Recommender.RDevice, r["inference.device"].Reason);
        Assert.Equal(["GPU", "150"], r["inference.device"].Args);
        Assert.Equal("280", r["inference.imageTokens"].Value);
        Assert.Equal("60", r["replay.seconds"].Value);
        Assert.Equal(Recommender.RSeconds, r["replay.seconds"].Reason);
        Assert.Equal("512", r["index.dim"].Value);
        Assert.Equal(Recommender.RDim, r["index.dim"].Reason);
        Assert.Equal("true", r["governance.pauseOnBattery"].Value);
        Assert.Equal("false", r["governance.pauseOnBattery"].Current);
        Assert.Equal(Recommender.RBattery, r["governance.pauseOnBattery"].Reason);
        Assert.False(r.ContainsKey("replay.fps"));                // 120 Hz panel: 120 and 60 break, 30 = current
        Assert.Equal("5500", r["replay.bitrateKbps"].Value);      // 2880x1800x30x0.035 = 5443
        // Retention 30 days: no dim change; battery already on: nothing emitted.
        var r2 = Run(Profiles.AmdLaptop());
        Assert.False(r2.ContainsKey("index.dim"));
        Assert.False(r2.ContainsKey("governance.pauseOnBattery"));
    }

    [Fact]
    public void Old_intel_without_av1()
    {
        var r = Run(Profiles.OldIntel());
        Assert.False(r.ContainsKey("inference.device"));
        Assert.Equal("70", r["inference.imageTokens"].Value);     // even 70 tokens: 2000 ms x 0.3/s = 60%
        Assert.Equal("3000", r["capture.intervalMs"].Value);      // 2000 ms x 0.2/s = 40%
        Assert.Equal(Recommender.RInterval, r["capture.intervalMs"].Reason);
        Assert.Equal(["3000", "2000", "0.2", "40"], r["capture.intervalMs"].Args);
        Assert.Equal("\"hevc\"", r["replay.encoder"].Value);
        Assert.Equal("\"perMonitor\"", r["replay.layout"].Value); // composited 18 ms breaks 16.7/2
        Assert.Equal(Recommender.RLayout, r["replay.layout"].Reason);
        Assert.False(r.ContainsKey("replay.fps"));                // 60 passes realtime but CPU 1.4% > 1%: 30
        Assert.Equal("3000", r["replay.bitrateKbps"].Value);      // per monitor 1920x1080x30x0.05
    }

    [Fact]
    public void No_hardware_encoder_disables_replay()
    {
        var r = Run(Profiles.NoEncoder());
        Assert.Equal("false", r["replay.enabled"].Value);
        Assert.Equal(Recommender.RUnsupported, r["replay.enabled"].Reason);
        Assert.False(r.ContainsKey("replay.encoder"));
        Assert.False(r.ContainsKey("inference.device"));          // CPU is the only device: auto already picks it
    }

    [Fact]
    public void Encoders_that_all_fail_disable_replay()
    {
        var rep = Profiles.OldIntel();
        rep = rep with { Encoders = rep.Encoders.Select(e => e with { Ok = false, EncodeMsP95 = 0, Error = "MFT init failed" }).ToArray() };
        Assert.Equal("false", Run(rep)["replay.enabled"].Value);
    }

    [Fact]
    public void Slow_encoder_that_scales_to_720p15_keeps_replay()
    {
        // 40 ms at 3840x1080 is ~17.8 ms at 2560x720: fails 30 fps, passes 24 fps (half period 20.8 ms).
        var rep = Profiles.OldIntel();
        rep = rep with { Encoders = rep.Encoders.Select(e => e with { EncodeMsP95 = 40 }).ToArray() };
        var r = Run(rep);
        Assert.False(r.ContainsKey("replay.enabled"));
        Assert.Equal("\"720p\"", r["replay.resolution"].Value);
        Assert.Equal("24", r["replay.fps"].Value);                // 17.8 ms < 20.8 ms (half of 41.7)
    }

    [Fact]
    public void Lower_resolution_when_native_fails()
    {
        var r = Run(Profiles.Ultrawide());
        Assert.Equal("\"1080p\"", r["replay.resolution"].Value);
        Assert.Equal(Recommender.RResolution, r["replay.resolution"].Reason);
        Assert.False(r.ContainsKey("replay.layout"));             // stays composited
        Assert.False(r.ContainsKey("inference.device"));          // no inference result: no inference rules
    }

    [Fact]
    public void Only_differences_are_emitted()
    {
        var rep = Profiles.ThisPc();
        var s = new Settings();
        foreach (var rec in Recommender.Recommend(rep, s)) Apply(s, rec);
        Assert.Empty(Recommender.Recommend(rep, s));
    }

    [Fact]
    public void Every_reason_code_is_covered_by_a_profile()
    {
        var seen = new HashSet<string>();
        var laptop = new Settings();
        laptop.Retention.Days = 90;
        laptop.Governance.PauseOnBattery = false;
        foreach (var (rep, s) in new[] { (Profiles.ThisPc(), new Settings()), (Profiles.NvidiaDesktop(), new Settings()), (Profiles.AmdLaptop(), laptop),
                                          (Profiles.OldIntel(), new Settings()), (Profiles.NoEncoder(), new Settings()), (Profiles.Ultrawide(), new Settings()),
                                          (Profiles.MultiGpu(), new Settings()) })
            foreach (var rec in Recommender.Recommend(rep, s)) seen.Add(rec.Reason);
        Assert.Equal(new[]
        {
            Recommender.RDevice, Recommender.RTokens, Recommender.RInterval, Recommender.REncoder, Recommender.RLayout, Recommender.RFps,
            Recommender.RResolution, Recommender.RBitrate, Recommender.RSeconds, Recommender.RUnsupported, Recommender.RDim, Recommender.RBattery,
            Recommender.RGpuAdapter,
        }.Order(), seen.Order());
    }

    private static void Apply(Settings s, Recommendation r)
    {
        var json = System.Text.Json.JsonDocument.Parse($"{{\"{r.Key.Split('.')[0]}\":{{\"{r.Key.Split('.')[1]}\":{r.Value}}}}}").RootElement;
        var home = new TempHome(null);
        try
        {
            var store = SettingsStore.Load(home.Path);
            Assert.Null(store.Replace(s));
            Assert.Null(store.TryUpdate(json)); // every recommended value passes settings validation
            var cur = store.Current;
            s.Inference = cur.Inference; s.Capture = cur.Capture; s.Replay = cur.Replay; s.Index = cur.Index; s.Governance = cur.Governance;
        }
        finally { home.Dispose(); }
    }
}
