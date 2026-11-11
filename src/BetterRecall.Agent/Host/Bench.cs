// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Host;

internal sealed record EncoderBenchSpec(AdapterInfo Adapter, VideoCodec Codec, string Api, string Layout, string Resolution, int Fps);

/// <summary>Everything the benchmark touches, so the orchestration can be tested with fakes.</summary>
internal interface IBenchTarget
{
    HardwareInfo Hardware();
    Task<BenchResult?> WorkerBenchAsync(CancellationToken ct);
    /// <summary>Usable adapter x codec pairs with the API to use (first available of EncoderPlan.Apis).</summary>
    IReadOnlyList<(AdapterInfo Adapter, VideoCodec Codec, string Api)> EncoderOptions();
    int MaxRefreshHz();
    int MonitorCount();
    void PauseReplay();
    void ResumeReplay();
    EncoderBench RunEncoder(EncoderBenchSpec spec, TimeSpan duration, CancellationToken ct);
    CaptureBench? RunCapture(CancellationToken ct);
}

internal static class BenchRunner
{
    public const int ProbeFps = 60;
    public const int FallbackFps = 30;
    /// <summary>Error prefix of a run whose encoder did not keep up with the requested frame rate.</summary>
    public const string SlowPrefix = "keeps up at ";

    /// <summary>
    /// Adaptive encoder plan, a handful of runs instead of a full grid (the recommender scales from the nearest row):
    /// 1. every adapter x codec option: composited, native size, 60 fps; 30 fps when 60 does not keep up;
    /// 2. per adapter, its first passing option per monitor (only with more than one monitor);
    /// 3. the first option that held 60 fps at the highest refresh rate, when that is above 60.
    /// Every run feeds moving synthetic content, so a static desktop does not make encoders look free.
    /// </summary>
    public static async Task<BenchReport> RunAsync(IBenchTarget target, TimeSpan perRun, Func<DateTimeOffset> clock, CancellationToken ct)
    {
        var sw = Stopwatch.StartNew();
        var ts = clock();

        var hardware = target.Hardware();

        BenchResult? inference = null;
        try
        {
            inference = await target.WorkerBenchAsync(ct).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            throw;
        }
        catch (Exception ex)
        {
            Log.Warn("worker bench", ex);
        }
        Log.Info($"bench: inference done at {sw.ElapsedMilliseconds} ms");

        var encoders = new List<EncoderBench>();
        CaptureBench? capture = null;

        EncoderBench Run(EncoderBenchSpec spec)
        {
            ct.ThrowIfCancellationRequested();
            EncoderBench r;
            try { r = target.RunEncoder(spec, perRun, ct); }
            catch (OperationCanceledException) { throw; }
            catch (Exception ex)
            {
                r = new EncoderBench(spec.Adapter.Name, EncoderFactory.CodecName(spec.Codec), spec.Api, spec.Layout,
                    0, 0, spec.Fps, false, 0, 0, 0, ex.Message, spec.Resolution);
            }
            encoders.Add(r);
            Log.Info($"bench encoder {spec.Adapter.Name} {spec.Api} {r.Codec} {spec.Layout} {r.Width}x{r.Height}@{spec.Fps}: " +
                     (r.Ok ? $"p95 {r.EncodeMsP95:F1} ms, cpu {r.CpuPercent:F1}%" : $"failed: {r.Error}"));
            return r;
        }

        target.PauseReplay();
        try
        {
            var passed = new List<((AdapterInfo Adapter, VideoCodec Codec, string Api) Option, int Fps)>();
            foreach (var opt in target.EncoderOptions())
            {
                var r = Run(new EncoderBenchSpec(opt.Adapter, opt.Codec, opt.Api, "composited", "native", ProbeFps));
                int okFps = r.Ok ? ProbeFps : 0;
                if (!r.Ok && r.Error?.StartsWith(SlowPrefix, StringComparison.Ordinal) == true &&
                    Run(new EncoderBenchSpec(opt.Adapter, opt.Codec, opt.Api, "composited", "native", FallbackFps)).Ok)
                    okFps = FallbackFps;
                if (okFps > 0) passed.Add((opt, okFps));
            }

            if (target.MonitorCount() > 1)
                foreach (var g in passed.GroupBy(p => p.Option.Adapter.Luid))
                {
                    var (opt, fps) = g.First();
                    Run(new EncoderBenchSpec(opt.Adapter, opt.Codec, opt.Api, "perMonitor", "native", fps));
                }

            int maxHz = target.MaxRefreshHz();
            if (maxHz > ProbeFps && passed.FirstOrDefault(p => p.Fps == ProbeFps) is { Option.Api: not null } top)
                Run(new EncoderBenchSpec(top.Option.Adapter, top.Option.Codec, top.Option.Api, "composited", "native", maxHz));
            Log.Info($"bench: {encoders.Count} encoder runs done at {sw.ElapsedMilliseconds} ms");

            try
            {
                capture = target.RunCapture(ct);
            }
            catch (OperationCanceledException)
            {
                throw;
            }
            catch (Exception ex)
            {
                Log.Warn("capture bench", ex);
            }
        }
        finally
        {
            target.ResumeReplay();
        }

        return new BenchReport(ts, sw.ElapsedMilliseconds, hardware, inference, encoders.ToArray(), capture, null, []);
    }
}

internal sealed class AgentBenchTarget : IBenchTarget
{
    private readonly Func<IReadOnlyList<OutputCapture>> _outputs;
    private readonly ReplayEngine _engine;
    private readonly Func<CancellationToken, Task<BenchResult?>> _workerBench;
    private readonly Func<Settings> _settings;
    private readonly GpuDeviceCache _devices;
    private readonly Dictionary<(long, VideoCodec, string, string, int, string), EncoderBench> _encoderCache = new();

    public AgentBenchTarget(
        CaptureService capture,
        Func<CancellationToken, Task<BenchResult?>> workerBench,
        Func<Settings> settings,
        GpuDeviceCache devices)
        : this(() => capture.Outputs, capture.Engine, workerBench, settings, devices) { }

    /// <summary>Without a CaptureService: the dev self-test drives its own captures and replay engine.</summary>
    public AgentBenchTarget(
        Func<IReadOnlyList<OutputCapture>> outputs,
        ReplayEngine engine,
        Func<CancellationToken, Task<BenchResult?>> workerBench,
        Func<Settings> settings,
        GpuDeviceCache devices)
    {
        _outputs = outputs;
        _engine = engine;
        _workerBench = workerBench;
        _settings = settings;
        _devices = devices;
    }

    public HardwareInfo Hardware() => HardwareProbe.Collect(_outputs().Select(o => o.Info).ToList());

    public Task<BenchResult?> WorkerBenchAsync(CancellationToken ct) => _workerBench(ct);

    public IReadOnlyList<(AdapterInfo Adapter, VideoCodec Codec, string Api)> EncoderOptions()
    {
        var list = new List<(AdapterInfo Adapter, VideoCodec Codec, string Api)>();
        var codecs = new[] { VideoCodec.Av1, VideoCodec.Hevc, VideoCodec.H264 };
        foreach (var adapter in GpuDevices.Adapters())
        {
            if (adapter.Software) continue;
            foreach (var codec in codecs)
            {
                foreach (var api in EncoderPlan.Apis(adapter, codec))
                {
                    if (EncoderFactory.Available(adapter, codec, api))
                    {
                        list.Add((adapter, codec, api));
                        break;
                    }
                }
            }
        }
        return list;
    }

    public int MonitorCount() => _outputs().Count(o => o.SourceWidth > 0 && !o.Disposed);

    public int MaxRefreshHz()
    {
        var outputs = _outputs();
        if (outputs.Count == 0) return 60;
        return outputs.Max(o => o.Info.RefreshHz);
    }

    public void PauseReplay()
    {
        _engine.Suspend();
    }

    public void ResumeReplay()
    {
        foreach (var o in _outputs())
        {
            o.ReplayPollFps = 0;
        }
        _engine.ResumeAfterSuspend();
    }

    public EncoderBench RunEncoder(EncoderBenchSpec spec, TimeSpan duration, CancellationToken ct)
    {
        var sources = _outputs().Where(o => o.SourceWidth > 0 && o.Device is not null && !o.Disposed).ToList();
        if (sources.Count == 0) throw new InvalidOperationException("no captured monitor");

        var groups = new List<(string Key, IReadOnlyList<OutputCapture> Group)>();
        if (string.Equals(spec.Layout, "perMonitor", StringComparison.OrdinalIgnoreCase))
        {
            for (int i = 0; i < sources.Count; i++)
            {
                groups.Add(($"bench:{sources[i].Info.Index}", [sources[i]]));
            }
        }
        else
        {
            groups.Add(("bench", sources));
        }

        double bpp = spec.Codec switch
        {
            VideoCodec.Av1 => 0.035,
            VideoCodec.Hevc => 0.05,
            _ => 0.08,
        };

        int FormulaKbps(int pixels, int fps)
        {
            double raw = pixels * (double)fps * bpp / 1000.0;
            int rounded = (int)Math.Round(raw / 500.0) * 500;
            return Math.Clamp(rounded, 500, 50000);
        }

        var streams = new List<VideoStream>();
        var pipelines = new List<ReplayPipeline>();
        string? createError = null;

        foreach (var (key, group) in groups)
        {
            var candidate = new EncoderCandidate(spec.Adapter, spec.Codec, spec.Api);
            var stream = new VideoStream(key, 64L << 20, TimeSpan.FromSeconds(10), false);
            streams.Add(stream);

            var pipeline = ReplayPipeline.Create(
                new PipelineSpec(key, group, candidate, spec.Fps, 6000, spec.Resolution, 100),
                stream,
                _devices,
                out var err);

            if (pipeline is null)
            {
                createError = err ?? "pipeline creation failed";
                break;
            }

            int pixels = pipeline.Layout.Width * pipeline.Layout.Height;
            int formula = FormulaKbps(pixels, spec.Fps);
            if (Math.Abs(formula - 6000) / 6000.0 > 0.10)
            {
                pipeline.Dispose();
                stream.Dispose();
                streams.RemoveAt(streams.Count - 1);

                stream = new VideoStream(key, 64L << 20, TimeSpan.FromSeconds(10), false);
                streams.Add(stream);

                pipeline = ReplayPipeline.Create(
                    new PipelineSpec(key, group, candidate, spec.Fps, formula, spec.Resolution, 100),
                    stream,
                    _devices,
                    out err);

                if (pipeline is null)
                {
                    createError = err ?? "pipeline creation failed";
                    break;
                }
            }

            pipelines.Add(pipeline);
        }

        if (createError is not null || pipelines.Count < groups.Count)
        {
            foreach (var p in pipelines) p.Dispose();
            foreach (var s in streams) s.Dispose();
            return new EncoderBench(
                spec.Adapter.Name,
                EncoderFactory.CodecName(spec.Codec),
                spec.Api,
                spec.Layout,
                0,
                0,
                spec.Fps,
                false,
                0,
                0,
                0,
                createError ?? "pipeline creation failed");
        }

        var sizesKey = string.Join(";", pipelines.Select(p => $"{p.Layout.Width}x{p.Layout.Height}"));
        var cacheKey = (spec.Adapter.Luid, spec.Codec, spec.Api, spec.Layout, spec.Fps, sizesKey);
        if (_encoderCache.TryGetValue(cacheKey, out var cached))
        {
            foreach (var p in pipelines) p.Dispose();
            foreach (var s in streams) s.Dispose();
            return cached;
        }

        try
        {
            foreach (var s in sources) s.ReplayPollFps = spec.Fps;
            foreach (var p in pipelines) p.SyntheticMotion = true;
            long packetsAtWarm = 0;

            using var timer = new HighResTimer();
            var sw = Stopwatch.StartNew();
            using var proc = Process.GetCurrentProcess();
            bool warmedUp = false;
            TimeSpan cpuStart = TimeSpan.Zero;
            long wallStartTicks = 0;

            while (sw.Elapsed < duration)
            {
                ct.ThrowIfCancellationRequested();
                long now = Stopwatch.GetTimestamp();

                if (!warmedUp && sw.Elapsed >= TimeSpan.FromMilliseconds(500))
                {
                    warmedUp = true;
                    foreach (var p in pipelines) p.Encoder.Latency.Reset();
                    packetsAtWarm = pipelines.Sum(p => p.Encoder.PacketsOut);
                    foreach (var p in pipelines) p.MotionTicksAtWarm = p.MotionTicks;
                    cpuStart = proc.TotalProcessorTime;
                    wallStartTicks = Stopwatch.GetTimestamp();
                }

                foreach (var p in pipelines)
                {
                    if (p.Pacer.TryTick(now, out long pts))
                    {
                        p.Tick(pts, [], forceAll: false); // real dirty rects plus the synthetic motion, like a busy desktop
                    }
                }

                long earliestDeadline = long.MaxValue;
                foreach (var p in pipelines)
                {
                    long dl = p.Pacer.NextDeadline;
                    if (dl < earliestDeadline) earliestDeadline = dl;
                }

                now = Stopwatch.GetTimestamp();
                if (earliestDeadline > now)
                {
                    long hundredNs = FramePacer.ToHundredNs(earliestDeadline - now, Stopwatch.Frequency);
                    timer.Sleep(hundredNs);
                }
            }

            proc.Refresh();
            TimeSpan cpuElapsed = warmedUp ? (proc.TotalProcessorTime - cpuStart) : proc.TotalProcessorTime;
            // Painting the synthetic content is benchmark overhead, not replay cost.
            cpuElapsed -= TimeSpan.FromSeconds(pipelines.Sum(p => p.MotionTicks - p.MotionTicksAtWarm) / (double)Stopwatch.Frequency);
            if (cpuElapsed < TimeSpan.Zero) cpuElapsed = TimeSpan.Zero;
            double wallMs = warmedUp ? ((Stopwatch.GetTimestamp() - wallStartTicks) * 1000.0 / Stopwatch.Frequency) : sw.Elapsed.TotalMilliseconds;
            double cpuPercent = wallMs > 0 ? (cpuElapsed.TotalMilliseconds / wallMs * 100.0) : 0.0;

            double encodeMsP95 = pipelines.Count > 0 ? pipelines.Max(p => p.Encoder.Latency.Percentile(95)) : 0.0;
            long workingSetMb = proc.WorkingSet64 / (1024 * 1024);
            int width = pipelines[0].Layout.Width;
            int height = pipelines[0].Layout.Height;

            bool ok = true;
            string? error = null;
            // Frames per encoder per second after warm-up; below 90 % of the target the encoder does not keep up.
            double achieved = wallMs > 0 ? (pipelines.Sum(p => p.Encoder.PacketsOut) - packetsAtWarm) / (double)pipelines.Count / (wallMs / 1000.0) : 0;
            if (achieved < spec.Fps * 0.9)
            {
                ok = false;
                error = $"{BenchRunner.SlowPrefix}{achieved:F0} of {spec.Fps} fps";
            }
            foreach (var p in pipelines)
            {
                if (p.Encoder.PacketsOut <= 0)
                {
                    ok = false;
                    error = p.Encoder.Error ?? "no packets produced";
                    break;
                }
                if (!string.IsNullOrEmpty(p.Encoder.Error))
                {
                    ok = false;
                    error = p.Encoder.Error;
                    break;
                }
            }

            var result = new EncoderBench(
                spec.Adapter.Name,
                EncoderFactory.CodecName(spec.Codec),
                spec.Api,
                spec.Layout,
                width,
                height,
                spec.Fps,
                ok,
                encodeMsP95,
                cpuPercent,
                workingSetMb,
                error,
                spec.Resolution);

            _encoderCache[cacheKey] = result;
            return result;
        }
        finally
        {
            foreach (var s in sources) s.ReplayPollFps = 0;
            foreach (var p in pipelines) p.Dispose();
            foreach (var s in streams) s.Dispose();
        }
    }

    public CaptureBench? RunCapture(CancellationToken ct)
    {
        var settings = _settings();
        var outputs = _outputs().Where(o => o.SourceWidth > 0 && o.Device is not null && !o.Disposed).ToList();
        if (outputs.Count == 0) return null;

        var collectedGrabs = new List<KeyframeGrab>();
        var grabLock = new object();
        var initialStats = outputs.Select(o => (o, o.GrabTicks, o.GrabCount)).ToList();

        foreach (var o in outputs)
        {
            o.GrabOverride = grab =>
            {
                lock (grabLock) collectedGrabs.Add(grab);
            };
        }

        try
        {
            int maxWidth = Math.Clamp(settings.Capture.MaxWidth, 640, 3840);
            for (int i = 0; i < 3; i++)
            {
                ct.ThrowIfCancellationRequested();
                var req = new KeyframeRequest(DateTimeOffset.UtcNow, "", "", "", "", true, maxWidth, 0);
                foreach (var o in outputs)
                {
                    o.PendingKeyframe = req;
                }
                if (i < 2)
                {
                    for (int s = 0; s < 6; s++)
                    {
                        ct.ThrowIfCancellationRequested();
                        Thread.Sleep(50);
                    }
                }
            }

            long waitStart = Stopwatch.GetTimestamp();
            int expected = outputs.Count * 3;
            while (Stopwatch.GetTimestamp() - waitStart < Stopwatch.Frequency * 3)
            {
                ct.ThrowIfCancellationRequested();
                lock (grabLock)
                {
                    if (collectedGrabs.Count >= expected) break;
                }
                Thread.Sleep(50);
            }

            double sumTickMs = 0;
            int activeOutputs = 0;
            foreach (var (o, initTicks, initCount) in initialStats)
            {
                long dTicks = o.GrabTicks - initTicks;
                long dCount = o.GrabCount - initCount;
                if (dCount > 0)
                {
                    sumTickMs += (dTicks * 1000.0 / Stopwatch.Frequency) / dCount;
                    activeOutputs++;
                }
            }
            double tickMs = activeOutputs > 0 ? sumTickMs / activeOutputs : 0;

            double totalEncodeMs = 0;
            int encodeCount = 0;
            lock (grabLock)
            {
                foreach (var grab in collectedGrabs)
                {
                    try
                    {
                        long t0 = Stopwatch.GetTimestamp();
                        unsafe
                        {
                            _ = ImageCodec.EncodeBgra(grab.Pixels, grab.Width, grab.Height, grab.Stride, settings.Keyframes.Format, settings.Keyframes.Quality);
                        }
                        totalEncodeMs += (Stopwatch.GetTimestamp() - t0) * 1000.0 / Stopwatch.Frequency;
                        encodeCount++;
                    }
                    finally
                    {
                        grab.Dispose();
                    }
                }
                collectedGrabs.Clear();
            }
            double keyframeEncodeMs = encodeCount > 0 ? totalEncodeMs / encodeCount : 0;

            double crossAdapterMs = 0;
            var options = EncoderOptions();
            if (options.Count > 0)
            {
                var first = options[0];
                VideoStream? crossStream = null;
                ReplayPipeline? crossPipeline = null;
                try
                {
                    crossStream = new VideoStream("bench:cross", 64L << 20, TimeSpan.FromSeconds(10), false);
                    var spec = new PipelineSpec("bench:cross", outputs, new EncoderCandidate(first.Adapter, first.Codec, first.Api), 30, 6000, "native", 100);
                    crossPipeline = ReplayPipeline.Create(spec, crossStream, _devices, out _);
                    if (crossPipeline is not null)
                    {
                        foreach (var o in outputs) o.ReplayPollFps = 30;
                        using var timer = new HighResTimer();
                        var sw = Stopwatch.StartNew();
                        while (sw.Elapsed < TimeSpan.FromSeconds(2))
                        {
                            ct.ThrowIfCancellationRequested();
                            long now = Stopwatch.GetTimestamp();
                            if (crossPipeline.Pacer.TryTick(now, out long pts))
                            {
                                crossPipeline.Tick(pts, [], forceAll: true);
                            }
                            long deadline = crossPipeline.Pacer.NextDeadline;
                            now = Stopwatch.GetTimestamp();
                            if (deadline > now)
                            {
                                long hundredNs = FramePacer.ToHundredNs(deadline - now, Stopwatch.Frequency);
                                timer.Sleep(hundredNs);
                            }
                        }
                        if (crossPipeline.CopyCount > 0)
                        {
                            crossAdapterMs = (crossPipeline.CopyTicks * 1000.0 / Stopwatch.Frequency) / crossPipeline.CopyCount;
                        }
                    }
                }
                finally
                {
                    foreach (var o in outputs) o.ReplayPollFps = 0;
                    crossPipeline?.Dispose();
                    crossStream?.Dispose();
                }
            }

            return new CaptureBench(tickMs, keyframeEncodeMs, crossAdapterMs);
        }
        finally
        {
            foreach (var o in outputs)
            {
                o.GrabOverride = null;
            }
            lock (grabLock)
            {
                foreach (var g in collectedGrabs) g.Dispose();
                collectedGrabs.Clear();
            }
        }
    }
}
