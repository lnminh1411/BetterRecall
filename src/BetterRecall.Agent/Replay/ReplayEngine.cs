// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;

namespace BetterRecall.Agent.Replay;

internal sealed record ReplayEngineConfig(
    bool Enabled,
    string Layout,
    int Fps,
    int Kbps,
    int Seconds,
    string Encoder,
    string EncoderAdapter,
    string Resolution,
    int ScalePercent,
    bool LockMemory);

internal sealed class ReplayEngine : IDisposable
{
    private readonly GpuDeviceCache _devices;
    private readonly Func<IReadOnlyList<OutputCapture>> _outputs;
    private readonly Func<RECT[]> _masks;
    private readonly HighResTimer _timer = new();
    private readonly object _gate = new();
    private readonly Thread _thread;

    private volatile bool _stop;
    private volatile bool _paused;
    private volatile bool _suspended;
    private volatile bool _ticking;

    private ReplayEngineConfig? _currentConfig;
    private ReplayEngineConfig? _pendingConfig;
    private long _pendingConfigTime;

    private volatile bool _rebuildRequested;
    private volatile bool _panicRequested;
    private string? _rebuildReason;
    private long _lastBuildAttempt;
    private string? _lastError;

    private readonly List<ReplayPipeline> _pipelines = new();
    private readonly Dictionary<string, VideoStream> _streams = new();
    private List<OutputSignature> _lastSignatures = new();

    public ReplayEngine(GpuDeviceCache devices, Func<IReadOnlyList<OutputCapture>> outputs, Func<RECT[]> masks)
    {
        _devices = devices;
        _outputs = outputs;
        _masks = masks;
        _thread = new Thread(Run)
        {
            Name = "replay-engine",
            Priority = ThreadPriority.AboveNormal,
            IsBackground = true,
        };
    }

    public void Start() => _thread.Start();

    public void Configure(ReplayEngineConfig cfg)
    {
        lock (_gate)
        {
            _pendingConfig = cfg;
            _pendingConfigTime = Stopwatch.GetTimestamp();
        }
    }

    public bool Paused
    {
        get => _paused;
        set => _paused = value;
    }

    public void Suspend()
    {
        _suspended = true;
        long start = Stopwatch.GetTimestamp();
        while (_ticking && FramePacer.ToHundredNs(Stopwatch.GetTimestamp() - start, Stopwatch.Frequency) < 20_000_000L)
        {
            Thread.Sleep(10);
        }
    }

    public void ResumeAfterSuspend()
    {
        _suspended = false;
        RequestRebuild("resume");
    }

    public void RequestRebuild(string why)
    {
        lock (_gate)
        {
            _rebuildRequested = true;
            _rebuildReason = why;
        }
    }

    public IReadOnlyList<ReplayPipeline> Pipelines
    {
        get
        {
            lock (_gate) return _pipelines.ToList();
        }
    }

    public IReadOnlyList<VideoStream> Streams
    {
        get
        {
            lock (_gate) return _streams.Values.ToList();
        }
    }

    public string? LastError
    {
        get
        {
            lock (_gate) return _lastError;
        }
    }

    public int Save(Stream output, AudioRingSource? audio, int seconds)
    {
        IReadOnlyList<VideoStream> streams;
        lock (_gate) streams = Streams;
        return ReplayMux.Save(output, streams, audio, seconds);
    }

    /// <summary>Zeroes every ring now; the engine thread then rebuilds (fresh encoders, black canvas).</summary>
    public void Panic()
    {
        lock (_gate)
        {
            foreach (var s in _streams.Values) s.Clear();
        }
        _panicRequested = true;
        RequestRebuild("panic");
    }

    public long RingBytes()
    {
        lock (_gate)
        {
            long sum = 0;
            foreach (var s in _streams.Values) sum += s.Ring.CommittedBytes;
            return sum;
        }
    }

    public void Dispose()
    {
        _stop = true;
        if (_thread.IsAlive) _thread.Join(3000);
        DisposePipelines();
        lock (_gate)
        {
            foreach (var s in _streams.Values) s.Dispose();
            _streams.Clear();
        }
        _timer.Dispose();
    }

    private void DisposePipelines()
    {
        List<ReplayPipeline> old;
        lock (_gate)
        {
            old = _pipelines.ToList();
            _pipelines.Clear();
        }
        foreach (var p in old) p.Dispose();
    }

    private void Run()
    {
        while (!_stop)
        {
            try
            {
                // Panic: drop encoders and canvas now, even while paused; a running engine rebuilds next.
                if (_panicRequested)
                {
                    _panicRequested = false;
                    DisposePipelines();
                }
                ApplyPendingConfigIfNeeded();

                bool notRunning;
                lock (_gate)
                {
                    notRunning = _currentConfig is null || !_currentConfig.Enabled || _paused || _suspended;
                }

                if (notRunning)
                {
                    var outs = _outputs();
                    foreach (var o in outs) o.ReplayPollFps = 0;
                    _ticking = false;
                    Thread.Sleep(100);
                    continue;
                }

                CheckRebuild();

                TickPipelines();
            }
            catch (Exception ex)
            {
                Log.Warn("replay engine loop", ex);
                Thread.Sleep(100);
            }
        }
    }

    private void ApplyPendingConfigIfNeeded()
    {
        ReplayEngineConfig? pending = null;
        lock (_gate)
        {
            if (_pendingConfig is not null)
            {
                bool immediate = _pipelines.Count == 0 ||
                                 _currentConfig is null ||
                                 _pendingConfig.Enabled != _currentConfig.Enabled ||
                                 !string.Equals(_pendingConfig.Layout, _currentConfig.Layout, StringComparison.OrdinalIgnoreCase);

                if (immediate)
                {
                    pending = _pendingConfig;
                    _pendingConfig = null;
                }
                else
                {
                    long nowTicks = Stopwatch.GetTimestamp();
                    long elapsed100ns = FramePacer.ToHundredNs(nowTicks - _pendingConfigTime, Stopwatch.Frequency);
                    bool gopBoundary = true;
                    foreach (var p in _pipelines)
                    {
                        long halfFrame = 5_000_000L / p.Spec.Fps;
                        long now100ns = FramePacer.ToHundredNs(nowTicks, Stopwatch.Frequency);
                        if (now100ns - p.Stream.LastKeyPts < 10_000_000L - 2 * halfFrame)
                        {
                            gopBoundary = false;
                            break;
                        }
                    }
                    if (gopBoundary || elapsed100ns >= 15_000_000L)
                    {
                        pending = _pendingConfig;
                        _pendingConfig = null;
                    }
                }

                if (pending is not null)
                {
                    _currentConfig = pending;
                    DisposePipelines();
                    Build();
                }
            }
        }
    }

    private void CheckRebuild()
    {
        var currentOutputs = _outputs();
        var currentSigs = currentOutputs
            .Select(o => new OutputSignature(o, o.Info.Index, o.Info.Left, o.Info.Top, o.SourceWidth, o.SourceHeight, o.FeedGeneration))
            .ToList();

        bool sigsChanged = !_lastSignatures.SequenceEqual(currentSigs);
        bool anyNeedsRebuild = false;
        lock (_gate)
        {
            foreach (var p in _pipelines)
            {
                if (p.NeedsRebuild)
                {
                    anyNeedsRebuild = true;
                    break;
                }
            }
        }

        bool shouldRebuild = _rebuildRequested || anyNeedsRebuild || sigsChanged;
        if (shouldRebuild)
        {
            long now = Stopwatch.GetTimestamp();
            long elapsedSinceBuild = FramePacer.ToHundredNs(now - _lastBuildAttempt, Stopwatch.Frequency);
            lock (_gate)
            {
                if (_pipelines.Count > 0 || elapsedSinceBuild >= 10_000_000L)
                {
                    _rebuildRequested = false;
                    DisposePipelines();
                    Build();
                }
            }
        }
    }

    private void Build()
    {
        _lastBuildAttempt = Stopwatch.GetTimestamp();
        var cfg = _currentConfig;
        if (cfg is null || !cfg.Enabled) return;

        var currentOutputs = _outputs();
        _lastSignatures = currentOutputs
            .Select(o => new OutputSignature(o, o.Info.Index, o.Info.Left, o.Info.Top, o.SourceWidth, o.SourceHeight, o.FeedGeneration))
            .ToList();

        var outputs = currentOutputs.Where(o => o.SourceWidth > 0 && o.Device is not null && !o.Disposed).ToList();
        if (outputs.Count == 0)
        {
            _lastError = "source not ready";
            return;
        }

        var candidates = EncoderPlan.Candidates(
            GpuDevices.Adapters(),
            outputs.Select(o => o.Info).ToList(),
            cfg.EncoderAdapter,
            cfg.Encoder,
            EncoderFactory.Available);

        if (candidates.Count == 0)
        {
            _lastError = "no hardware encoder";
            return;
        }

        bool perMonitor = string.Equals(cfg.Layout, "perMonitor", StringComparison.OrdinalIgnoreCase);
        var groups = new List<(string Key, IReadOnlyList<OutputCapture> Group, int Fps)>();
        if (perMonitor)
        {
            foreach (var o in outputs)
            {
                string key = "mon:" + o.Info.DeviceName;
                int fps = ReplayRate.Effective(cfg.Fps, o.Info.RefreshHz);
                groups.Add((key, new[] { o }, fps));
            }
        }
        else
        {
            string key = "canvas";
            int maxRefresh = outputs.Max(o => o.Info.RefreshHz);
            int fps = ReplayRate.Effective(cfg.Fps, maxRefresh);
            groups.Add((key, outputs, fps));
        }

        var newPipelines = new List<ReplayPipeline>();
        string? failureError = null;

        foreach (var (key, group, fps) in groups)
        {
            long capacity = (long)cfg.Kbps * 1000 / 8 * (cfg.Seconds + 2) * 21 / 20;
            if (!_streams.TryGetValue(key, out var stream) || stream.Ring.Capacity != capacity)
            {
                stream?.Dispose();
                stream = new VideoStream(key, capacity, TimeSpan.FromSeconds(cfg.Seconds), cfg.LockMemory);
                _streams[key] = stream;
            }

            ReplayPipeline? pipeline = null;
            string? err = null;
            foreach (var candidate in candidates)
            {
                var spec = new PipelineSpec(key, group, candidate, fps, cfg.Kbps, cfg.Resolution, cfg.ScalePercent);
                pipeline = ReplayPipeline.Create(spec, stream, _devices, out err);
                if (pipeline is not null) break;
                Log.Info($"replay pipeline candidate {candidate.Codec}/{candidate.Api} failed for {key}: {err}");
            }

            if (pipeline is null)
            {
                failureError = err ?? "pipeline creation failed";
                break;
            }
            newPipelines.Add(pipeline);
        }

        if (failureError is not null)
        {
            _lastError = failureError;
            foreach (var p in newPipelines) p.Dispose();
            return;
        }

        _lastError = null;
        var activeKeys = new HashSet<string>(groups.Select(g => g.Key));
        var unused = _streams.Keys.Where(k => !activeKeys.Contains(k)).ToList();
        foreach (var k in unused)
        {
            _streams[k].Dispose();
            _streams.Remove(k);
        }

        _pipelines.AddRange(newPipelines);

        foreach (var p in _pipelines)
        {
            foreach (var src in p.Spec.Sources)
            {
                src.ReplayPollFps = p.Spec.Fps;
            }
            Log.Info($"replay pipeline {p.Spec.Key}: {p.Encoder.Codec} {p.Encoder.Api} '{p.Encoder.Name}' {p.Layout.Width}x{p.Layout.Height}@{p.Spec.Fps} {p.Spec.Kbps} kbps");
        }
    }

    private void TickPipelines()
    {
        List<ReplayPipeline> snapshot;
        lock (_gate) snapshot = _pipelines.ToList();

        if (snapshot.Count > 0)
        {
            _ticking = true;
            long now = Stopwatch.GetTimestamp();
            var masks = _masks();

            foreach (var p in snapshot)
            {
                // Outputs poll at the pipeline rate again after a pause (the paused loop set them to 0).
                foreach (var src in p.Spec.Sources)
                    if (src.ReplayPollFps != p.Spec.Fps) src.ReplayPollFps = p.Spec.Fps;
                try
                {
                    if (p.Pacer.TryTick(now, out var pts))
                    {
                        p.Tick(pts, masks, false);
                    }
                }
                catch (Exception ex)
                {
                    Log.Warn($"replay tick {p.Spec.Key}", ex);
                }
            }

            long earliestDeadline = long.MaxValue;
            foreach (var p in snapshot)
            {
                long dl = p.Pacer.NextDeadline;
                if (dl < earliestDeadline) earliestDeadline = dl;
            }

            long maxDeadline = now + Stopwatch.Frequency / 10;
            long target = Math.Min(earliestDeadline, maxDeadline);
            if (target > now)
            {
                long hundredNs = FramePacer.ToHundredNs(target - now, Stopwatch.Frequency);
                _timer.Sleep(hundredNs);
            }
        }
        else
        {
            _ticking = false;
            Thread.Sleep(100);
        }
    }

    private readonly record struct OutputSignature(
        OutputCapture Output,
        int Index,
        int Left,
        int Top,
        int Width,
        int Height,
        int Generation);
}
