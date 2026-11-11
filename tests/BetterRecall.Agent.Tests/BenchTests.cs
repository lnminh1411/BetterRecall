// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Host;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Replay;
using BetterRecall.Contracts;
using Xunit;

namespace BetterRecall.Agent.Tests;

public class BenchTests
{
    [Fact]
    public void EncoderPlan_Candidates_SelectionOrder()
    {
        // (a) Intel iGPU (Integrated) + NVIDIA + AMD
        // Intel has av1:vpl, hevc:mf, h264:mf; NVIDIA hevc:mf, h264:mf; AMD hevc:mf, h264:mf
        // outputs: 1 on AMD, 2 on NVIDIA; setting auto/auto ->
        // Intel av1 vpl, Intel hevc mf, Intel h264 mf, then NVIDIA (drives more monitors) hevc, h264, then AMD hevc, h264.
        var intel = new AdapterInfo(1, GpuDevices.VendorIntel, "Intel Iris Xe", false, Integrated: true, VramMb: 0);
        var nvidia = new AdapterInfo(2, GpuDevices.VendorNvidia, "NVIDIA GeForce RTX 4080", false, Integrated: false, VramMb: 16384);
        var amd = new AdapterInfo(3, GpuDevices.VendorAmd, "AMD Radeon RX 7900", false, Integrated: false, VramMb: 24576);
        var adapters = new[] { intel, nvidia, amd };
        var outputs = new[]
        {
            new OutputInfo(0, "Display1", 3, GpuDevices.VendorAmd, "AMD Radeon RX 7900", 0, 0, 1920, 1080, 0),
            new OutputInfo(1, "Display2", 2, GpuDevices.VendorNvidia, "NVIDIA GeForce RTX 4080", 1920, 0, 1920, 1080, 0),
            new OutputInfo(2, "Display3", 2, GpuDevices.VendorNvidia, "NVIDIA GeForce RTX 4080", 3840, 0, 1920, 1080, 0),
        };

        bool AvailableA(AdapterInfo a, VideoCodec c, string api) => (a.Luid, c, api) switch
        {
            (1, VideoCodec.Av1, "vpl") => true,
            (1, VideoCodec.Hevc, "mf") => true,
            (1, VideoCodec.H264, "mf") => true,
            (2, VideoCodec.Hevc, "mf") => true,
            (2, VideoCodec.H264, "mf") => true,
            (3, VideoCodec.Hevc, "mf") => true,
            (3, VideoCodec.H264, "mf") => true,
            _ => false,
        };

        var candidatesA = EncoderPlan.Candidates(adapters, outputs, "auto", "auto", AvailableA);
        var expectedA = new (long Luid, VideoCodec Codec, string Api)[]
        {
            (1, VideoCodec.Av1, "vpl"),
            (1, VideoCodec.Hevc, "mf"),
            (1, VideoCodec.H264, "mf"),
            (2, VideoCodec.Hevc, "mf"),
            (2, VideoCodec.H264, "mf"),
            (3, VideoCodec.Hevc, "mf"),
            (3, VideoCodec.H264, "mf"),
        };
        Assert.Equal(expectedA.Length, candidatesA.Count);
        for (int i = 0; i < expectedA.Length; i++)
        {
            Assert.Equal(expectedA[i].Luid, candidatesA[i].Adapter.Luid);
            Assert.Equal(expectedA[i].Codec, candidatesA[i].Codec);
            Assert.Equal(expectedA[i].Api, candidatesA[i].Api);
        }

        // (b) NVIDIA-only (not integrated) -> NVIDIA hevc mf, h264 mf (no vpl, AV1 only if available via mf)
        var adaptersB = new[] { nvidia };
        var outputsB = new[] { new OutputInfo(0, "Display1", 2, GpuDevices.VendorNvidia, "NVIDIA GeForce RTX 4080", 0, 0, 1920, 1080, 0) };
        bool AvailableB(AdapterInfo a, VideoCodec c, string api) => (a.Luid, c, api) switch
        {
            (2, VideoCodec.Hevc, "mf") => true,
            (2, VideoCodec.H264, "mf") => true,
            _ => false,
        };
        var candidatesB = EncoderPlan.Candidates(adaptersB, outputsB, "auto", "auto", AvailableB);
        var expectedB = new (long Luid, VideoCodec Codec, string Api)[]
        {
            (2, VideoCodec.Hevc, "mf"),
            (2, VideoCodec.H264, "mf"),
        };
        Assert.Equal(expectedB.Length, candidatesB.Count);
        for (int i = 0; i < expectedB.Length; i++)
        {
            Assert.Equal(expectedB[i].Luid, candidatesB[i].Adapter.Luid);
            Assert.Equal(expectedB[i].Codec, candidatesB[i].Codec);
            Assert.Equal(expectedB[i].Api, candidatesB[i].Api);
        }

        // (c) AMD-only with av1:mf available -> AMD av1 mf, hevc mf, h264 mf
        var adaptersC = new[] { amd };
        var outputsC = new[] { new OutputInfo(0, "Display1", 3, GpuDevices.VendorAmd, "AMD Radeon RX 7900", 0, 0, 1920, 1080, 0) };
        bool AvailableC(AdapterInfo a, VideoCodec c, string api) => (a.Luid, c, api) switch
        {
            (3, VideoCodec.Av1, "mf") => true,
            (3, VideoCodec.Hevc, "mf") => true,
            (3, VideoCodec.H264, "mf") => true,
            _ => false,
        };
        var candidatesC = EncoderPlan.Candidates(adaptersC, outputsC, "auto", "auto", AvailableC);
        var expectedC = new (long Luid, VideoCodec Codec, string Api)[]
        {
            (3, VideoCodec.Av1, "mf"),
            (3, VideoCodec.Hevc, "mf"),
            (3, VideoCodec.H264, "mf"),
        };
        Assert.Equal(expectedC.Length, candidatesC.Count);
        for (int i = 0; i < expectedC.Length; i++)
        {
            Assert.Equal(expectedC[i].Luid, candidatesC[i].Adapter.Luid);
            Assert.Equal(expectedC[i].Codec, candidatesC[i].Codec);
            Assert.Equal(expectedC[i].Api, candidatesC[i].Api);
        }

        // (d) no hardware encoder (available always false) -> empty list
        var candidatesD = EncoderPlan.Candidates(adapters, outputs, "auto", "auto", (_, _, _) => false);
        Assert.Empty(candidatesD);

        // (e) encoderAdapter "geforce" on (a) -> NVIDIA candidates first; encoder "h264" -> only h264 candidates in adapter order
        var candidatesE1 = EncoderPlan.Candidates(adapters, outputs, "geforce", "auto", AvailableA);
        var expectedE1 = new (long Luid, VideoCodec Codec, string Api)[]
        {
            (2, VideoCodec.Hevc, "mf"),
            (2, VideoCodec.H264, "mf"),
            (1, VideoCodec.Av1, "vpl"),
            (1, VideoCodec.Hevc, "mf"),
            (1, VideoCodec.H264, "mf"),
            (3, VideoCodec.Hevc, "mf"),
            (3, VideoCodec.H264, "mf"),
        };
        Assert.Equal(expectedE1.Length, candidatesE1.Count);
        for (int i = 0; i < expectedE1.Length; i++)
        {
            Assert.Equal(expectedE1[i].Luid, candidatesE1[i].Adapter.Luid);
            Assert.Equal(expectedE1[i].Codec, candidatesE1[i].Codec);
            Assert.Equal(expectedE1[i].Api, candidatesE1[i].Api);
        }

        var candidatesE2 = EncoderPlan.Candidates(adapters, outputs, "auto", "h264", AvailableA);
        var expectedE2 = new (long Luid, VideoCodec Codec, string Api)[]
        {
            (1, VideoCodec.H264, "mf"),
            (2, VideoCodec.H264, "mf"),
            (3, VideoCodec.H264, "mf"),
        };
        Assert.Equal(expectedE2.Length, candidatesE2.Count);
        for (int i = 0; i < expectedE2.Length; i++)
        {
            Assert.Equal(expectedE2[i].Luid, candidatesE2[i].Adapter.Luid);
            Assert.Equal(expectedE2[i].Codec, candidatesE2[i].Codec);
            Assert.Equal(expectedE2[i].Api, candidatesE2[i].Api);
        }
    }

    [Fact]
    public void CanvasLayout_PerMonitorScaling()
    {
        int maxW = 7680, maxH = 4320;
        var mon = new MonitorPlacement[] { new(0, 0, 0, 2560, 1440) };

        // native 2560x1440: H0 = min(1440, 1440) = 1440, W0 = round(2560 * 1440 / 1440) = 2560
        var lNative = CanvasLayout.Compute(mon, "native", 100, maxW, maxH);
        Assert.Equal(2560, lNative.Width);
        Assert.Equal(1440, lNative.Height);

        // 2160p 2560x1440: H0 = min(1440, 2160) = 1440 (never upscaled), W0 = round(2560 * 1440 / 1440) = 2560
        var l2160 = CanvasLayout.Compute(mon, "2160p", 100, maxW, maxH);
        Assert.Equal(2560, l2160.Width);
        Assert.Equal(1440, l2160.Height);

        // 1440p 2560x1440: H0 = min(1440, 1440) = 1440, W0 = round(2560 * 1440 / 1440) = 2560
        var l1440 = CanvasLayout.Compute(mon, "1440p", 100, maxW, maxH);
        Assert.Equal(2560, l1440.Width);
        Assert.Equal(1440, l1440.Height);

        // 1080p 1920x1080: H0 = min(1440, 1080) = 1080, W0 = round(2560 * 1080 / 1440) = 1920
        var l1080 = CanvasLayout.Compute(mon, "1080p", 100, maxW, maxH);
        Assert.Equal(1920, l1080.Width);
        Assert.Equal(1080, l1080.Height);

        // 900p 1600x900: H0 = min(1440, 900) = 900, W0 = round(2560 * 900 / 1440) = 1600
        var l900 = CanvasLayout.Compute(mon, "900p", 100, maxW, maxH);
        Assert.Equal(1600, l900.Width);
        Assert.Equal(900, l900.Height);

        // 720p 1280x720: H0 = min(1440, 720) = 720, W0 = round(2560 * 720 / 1440) = 1280
        var l720 = CanvasLayout.Compute(mon, "720p", 100, maxW, maxH);
        Assert.Equal(1280, l720.Width);
        Assert.Equal(720, l720.Height);

        // 540p 960x540: H0 = min(1440, 540) = 540, W0 = round(2560 * 540 / 1440) = 960
        var l540 = CanvasLayout.Compute(mon, "540p", 100, maxW, maxH);
        Assert.Equal(960, l540.Width);
        Assert.Equal(540, l540.Height);

        // scale 75 -> 1920x1080: H0 = min(1440, 1440 * 75 / 100) = 1080, W0 = round(2560 * 1080 / 1440) = 1920
        var lScale = CanvasLayout.Compute(mon, "scale", 75, maxW, maxH);
        Assert.Equal(1920, lScale.Width);
        Assert.Equal(1080, lScale.Height);

        // Composited canvas of 1920x1080 + 1280x1024 pair side by side at 720p
        // Arithmetic:
        // Placements: Monitor 0 at (0, 0, 1920, 1080); Monitor 1 at (1920, 0, 1280, 1024).
        // Bounding box: minLeft = 0, minTop = 0, maxRight = 1920 + 1280 = 3200, maxBottom = max(1080, 1024) = 1080.
        // bboxW = 3200, bboxH = 1080.
        // Under ReplayScaling.Fit(bboxW = 3200, bboxH = 1080, resolution = "720p", scalePercent = 100, maxW = 7680, maxH = 4320):
        // H0 = TargetHeight(1080, "720p", 100) = 720.
        // W0 = (int)Math.Round((double)3200 * 720 / 1080) = (int)Math.Round(2133.333...) = 2133.
        // Neither exceeds maxW (7680) or maxH (4320).
        // Round to even dimensions:
        // Width = Math.Max(2, 2133 & ~1) = 2132.
        // Height = Math.Max(2, 720 & ~1) = 720.
        var pair = new MonitorPlacement[]
        {
            new(0, 0, 0, 1920, 1080),
            new(1, 1920, 0, 1280, 1024),
        };
        var lPair = CanvasLayout.Compute(pair, "720p", 100, maxW, maxH);
        Assert.Equal(2132, lPair.Width);
        Assert.Equal(720, lPair.Height);
    }

    [Fact]
    public async Task RunAsync_AdaptivePlan_ProbesEachOptionThenPerMonitorThenMaxRefresh()
    {
        var fake = new FakeBenchTarget { OnMaxRefreshHz = () => 144, OnMonitorCount = () => 3 };
        var report = await BenchRunner.RunAsync(fake, TimeSpan.FromSeconds(1), () => DateTimeOffset.UtcNow, CancellationToken.None);

        var calls = fake.RunEncoderCalls;
        // 2 options at 60 fps composited, then one per-monitor run per adapter, then the top option at 144.
        Assert.Equal(5, calls.Count);
        Assert.All(calls.Take(2), c => Assert.True(c.Layout == "composited" && c.Fps == 60 && c.Resolution == "native"));
        Assert.Equal(["Intel", "NVIDIA"], calls.Skip(2).Take(2).Select(c => c.Adapter.Name));
        Assert.All(calls.Skip(2).Take(2), c => Assert.Equal("perMonitor", c.Layout));
        Assert.Equal(("Intel", 144, "composited"), (calls[4].Adapter.Name, calls[4].Fps, calls[4].Layout));
        Assert.Equal(5, report.Encoders.Length);
    }

    [Fact]
    public async Task RunAsync_SlowAt60_RetriesAt30_AndSingleMonitorSkipsPerMonitor()
    {
        var fake = new FakeBenchTarget { OnMonitorCount = () => 1 };
        fake.OnRunEncoder = (spec, d, ct) =>
        {
            bool slow = spec.Adapter.Name == "Intel" && spec.Fps == 60;
            return new EncoderBench(spec.Adapter.Name, EncoderFactory.CodecName(spec.Codec), spec.Api, spec.Layout, 1920, 1080, spec.Fps,
                !slow, 4.0, 5.0, 300, slow ? BenchRunner.SlowPrefix + "41 of 60 fps" : null);
        };
        await BenchRunner.RunAsync(fake, TimeSpan.FromSeconds(1), () => DateTimeOffset.UtcNow, CancellationToken.None);

        Assert.Equal([("Intel", 60), ("Intel", 30), ("NVIDIA", 60)], fake.RunEncoderCalls.Select(c => (c.Adapter.Name, c.Fps)));
    }

    private sealed class FakeBenchTarget : IBenchTarget
    {
        public int PauseCount { get; private set; }
        public int ResumeCount { get; private set; }
        public List<EncoderBenchSpec> RunEncoderCalls { get; } = new();

        public Func<HardwareInfo> OnHardware { get; set; } = () =>
            new HardwareInfo("FakeCPU", 4, 8, 16384, false, [], null, [], "Windows");

        public Func<CancellationToken, Task<BenchResult?>> OnWorkerBench { get; set; } = ct =>
            Task.FromResult<BenchResult?>(new BenchResult("npu", [new BenchDevice("npu", "OpenVINO", true, 10.0, 5.0, 0.95, null)]));

        public Func<IReadOnlyList<(AdapterInfo Adapter, VideoCodec Codec, string Api)>> OnEncoderOptions { get; set; } = () =>
        [
            (new AdapterInfo(1, GpuDevices.VendorIntel, "Intel", false), VideoCodec.Av1, "vpl"),
            (new AdapterInfo(2, GpuDevices.VendorNvidia, "NVIDIA", false), VideoCodec.Hevc, "mf"),
        ];

        public Func<int> OnMaxRefreshHz { get; set; } = () => 60;
        public Func<int> OnMonitorCount { get; set; } = () => 2;

        public Func<EncoderBenchSpec, TimeSpan, CancellationToken, EncoderBench> OnRunEncoder { get; set; } = (spec, duration, ct) =>
            new EncoderBench(spec.Adapter.Name, EncoderFactory.CodecName(spec.Codec), spec.Api, spec.Layout, 1920, 1080, spec.Fps, true, 4.5, 8.0, 400, null);

        public Func<CancellationToken, CaptureBench?> OnRunCapture { get; set; } = ct =>
            new CaptureBench(1.2, 3.4, 0.8);

        public HardwareInfo Hardware() => OnHardware();
        public Task<BenchResult?> WorkerBenchAsync(CancellationToken ct) => OnWorkerBench(ct);
        public IReadOnlyList<(AdapterInfo Adapter, VideoCodec Codec, string Api)> EncoderOptions() => OnEncoderOptions();
        public int MaxRefreshHz() => OnMaxRefreshHz();
        public int MonitorCount() => OnMonitorCount();
        public void PauseReplay() => PauseCount++;
        public void ResumeReplay() => ResumeCount++;
        public EncoderBench RunEncoder(EncoderBenchSpec spec, TimeSpan duration, CancellationToken ct)
        {
            RunEncoderCalls.Add(spec);
            return OnRunEncoder(spec, duration, ct);
        }
        public CaptureBench? RunCapture(CancellationToken ct) => OnRunCapture(ct);
    }

    [Fact]
    public async Task RunAsync_WhenRunEncoderThrowsForOneAdapter_RecordsOkFalseAndContinues()
    {
        var fake = new FakeBenchTarget();
        fake.OnRunEncoder = (spec, duration, ct) =>
        {
            if (spec.Adapter.Name == "Intel")
                throw new InvalidOperationException("Intel encoder crashed");
            return new EncoderBench(spec.Adapter.Name, EncoderFactory.CodecName(spec.Codec), spec.Api, spec.Layout, 1920, 1080, spec.Fps, true, 4.0, 5.0, 300, null);
        };

        var report = await BenchRunner.RunAsync(fake, TimeSpan.FromSeconds(1), () => DateTimeOffset.UtcNow, CancellationToken.None);

        // Intel fails its probe (not a keep-up failure, so no 30 fps retry); NVIDIA passes and gets a per-monitor run.
        Assert.Equal(3, report.Encoders.Length);
        foreach (var enc in report.Encoders)
        {
            if (enc.Adapter == "Intel")
            {
                Assert.False(enc.Ok);
                Assert.Equal("Intel encoder crashed", enc.Error);
            }
            else
            {
                Assert.True(enc.Ok);
            }
        }

        Assert.Equal(1, fake.PauseCount);
        Assert.Equal(1, fake.ResumeCount);
        Assert.Null(report.Search);
        Assert.Empty(report.Recommendations);
        Assert.NotNull(report.Inference);
        Assert.Equal("npu", report.Inference.Chosen);
    }

    [Fact]
    public async Task RunAsync_WhenWorkerBenchThrows_ContinuesWithNullInference()
    {
        var fake = new FakeBenchTarget();
        fake.OnWorkerBench = ct => throw new InvalidOperationException("Worker connection lost");

        var report = await BenchRunner.RunAsync(fake, TimeSpan.FromSeconds(1), () => DateTimeOffset.UtcNow, CancellationToken.None);

        Assert.Null(report.Inference);
        Assert.Equal(4, report.Encoders.Length); // 2 probes + 2 per-monitor runs
        Assert.Equal(1, fake.PauseCount);
        Assert.Equal(1, fake.ResumeCount);
    }

    [Fact]
    public async Task RunAsync_WhenCancelled_ThrowsAndResumesReplay()
    {
        var fake = new FakeBenchTarget();
        using var cts = new CancellationTokenSource();
        int callCount = 0;

        fake.OnRunEncoder = (spec, duration, ct) =>
        {
            callCount++;
            if (callCount == 3)
            {
                cts.Cancel();
                throw new OperationCanceledException(cts.Token);
            }
            return new EncoderBench(spec.Adapter.Name, EncoderFactory.CodecName(spec.Codec), spec.Api, spec.Layout, 1920, 1080, spec.Fps, true, 4.0, 5.0, 300, null);
        };

        await Assert.ThrowsAsync<OperationCanceledException>(() =>
            BenchRunner.RunAsync(fake, TimeSpan.FromSeconds(1), () => DateTimeOffset.UtcNow, cts.Token));

        Assert.Equal(1, fake.PauseCount);
        Assert.Equal(1, fake.ResumeCount);
    }

    [Fact]
    public async Task RunAsync_WhenRunCaptureThrows_CaptureNullAndResumesReplay()
    {
        var fake = new FakeBenchTarget();
        fake.OnRunCapture = ct => throw new InvalidOperationException("Capture failed");

        var report = await BenchRunner.RunAsync(fake, TimeSpan.FromSeconds(1), () => DateTimeOffset.UtcNow, CancellationToken.None);

        Assert.Null(report.Capture);
        Assert.Equal(1, fake.PauseCount);
        Assert.Equal(1, fake.ResumeCount);
    }
}
