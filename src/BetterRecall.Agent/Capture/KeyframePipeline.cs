// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Threading.Channels;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Scheduling;
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Capture;

/// <summary>
/// Receives downscaled grabs from the capture threads: smart capture (change map and dHash against the last stored
/// keyframe, CapturePolicy), then the CPU budget (EncodeGovernor) decides when and how each kept frame is encoded
/// (KeyframeEncoder), then IStore.AddScreenCapture. Runs on its own below-normal EcoQoS thread; grabs are zeroed after use.
/// </summary>
internal sealed class KeyframePipeline : IAsyncDisposable
{
    private readonly Channel<KeyframeGrab> _channel = Channel.CreateBounded<KeyframeGrab>(new BoundedChannelOptions(6)
    {
        FullMode = BoundedChannelFullMode.DropWrite,
        SingleReader = true,
    });
    private readonly Func<IStore?> _store;
    private readonly Func<Settings> _settings;
    private readonly Action<CaptureMeta> _stored;
    private readonly Dictionary<int, DedupFilter> _dedup = new();
    private readonly EncodeGovernor<Pending> _governor;
    private readonly Thread _thread;
    private readonly CancellationTokenSource _cts = new();
    private string _encoderName = "";
    private Task<bool>? _wait;

    // Measurements.
    public long Kept, SkippedSimilar, Dropped, Failed;
    public double EncodeMsTotal, HashMsTotal, GrabMsTotal, EncodeCpuMsTotal;
    public double LastEncodeMs;
    public ChangeResult LastChange;

    /// <summary>Receives replay samples (ReplaySample requests). Null: samples are dropped.</summary>
    public Replay.ReplaySampler? Sampler { get; set; }

    /// <summary>Encoder of the last keyframe, e.g. "libaom speed 7" or "libaom speed 8 (CPU budget)" (log, status, bench).</summary>
    public string Encoder => _encoderName;

    /// <summary>Keyframe encode CPU in the last 10 s and 60 s as a share of all logical processors (Task Manager %).</summary>
    public (double Last10Percent, double Last60Percent, int Waiting, EncodeLevel Level) Budget
    {
        get
        {
            lock (_governor)
            {
                var (u10, u60) = _governor.Used;
                double procs = Environment.ProcessorCount;
                return (u10 / (procs * 10_000) * 100, u60 / (procs * 60_000) * 100, _governor.Waiting, _governor.Level);
            }
        }
    }

    public long BudgetDeferred => _governor.Deferred;
    public long BudgetDropped => _governor.Dropped;
    public long BudgetWebP => _governor.WebPFallbacks;

    /// <summary>A kept frame waiting for its encode. Owns the grab.</summary>
    private sealed class Pending(KeyframeGrab grab, ulong hash, ChangeResult change, KeepReason reason) : IDisposable
    {
        public KeyframeGrab Grab { get; } = grab;
        public ulong Hash { get; } = hash;
        public ChangeResult Change { get; } = change;
        public KeepReason Reason { get; } = reason;
        public void Dispose() => Grab.Dispose();
    }

    public KeyframePipeline(Func<IStore?> store, Func<Settings> settings, Action<CaptureMeta> stored)
    {
        _store = store;
        _settings = settings;
        _stored = stored;
        _governor = new EncodeGovernor<Pending>(() => Environment.TickCount64, Environment.ProcessorCount, EncodeAndStore,
            (p, why) => Log.Warn($"keyframe dropped by the CPU budget ({why})"), KeyframeEncoder.InitialEstimateMs);
        _thread = new Thread(Run) { IsBackground = true, Name = "keyframes", Priority = ThreadPriority.BelowNormal };
        _thread.Start();
    }

    public void Post(KeyframeGrab grab)
    {
        if (!_channel.Writer.TryWrite(grab))
        {
            Interlocked.Increment(ref Dropped);
            grab.Dispose();
        }
    }

    public void ResetDedup()
    {
        lock (_dedup) foreach (var d in _dedup.Values) d.Reset();
    }

    public bool ContextChanged(int monitor, string context)
    {
        lock (_dedup) return _dedup.TryGetValue(monitor, out var d) && d.ContextDiffers(context);
    }

    private void Run()
    {
        try { Governance.ApplyEcoQosToCurrentThread(); } catch (Exception ex) { Log.Debug($"keyframe thread EcoQoS: {ex.Message}"); }
        var ct = _cts.Token;
        while (!ct.IsCancellationRequested)
        {
            if (_channel.Reader.TryRead(out var grab))
            {
                bool owned = false;
                try { owned = Process(grab); }
                catch (Exception ex)
                {
                    Interlocked.Increment(ref Failed);
                    Log.Warn("keyframe", ex);
                }
                finally { if (!owned) grab.Dispose(); }
                continue;
            }
            lock (_governor) _governor.Pump();
            try
            {
                // Wake for a new grab, or every 500 ms while frames wait for budget. One pending wait at a time
                // (single-reader channel): a wait that timed out is reused on the next pass.
                _wait ??= _channel.Reader.WaitToReadAsync(ct).AsTask();
                if (!_wait.Wait(_governor.Waiting > 0 ? 500 : Timeout.Infinite, ct)) continue;
                bool more = _wait.Result;
                _wait = null;
                if (!more) break;
            }
            catch (OperationCanceledException) { break; }
            catch (AggregateException) { break; }
        }
    }

    /// <summary>True when the grab was handed to the governor (it disposes it).</summary>
    private unsafe bool Process(KeyframeGrab g)
    {
        var s = _settings();
        var store = _store();
        if (store is null) return false;
        string sampleFormat = s.Keyframes.Format == "jpeg" ? "jpeg" : "webp";
        int quality = Math.Clamp(s.Keyframes.Quality, 30, 95);
        byte[]? sampleBytes = null;
        if (g.Request.ReplaySample && Sampler is { } sampler)
        {
            sampleBytes = ImageCodec.EncodeBgra(g.Pixels, g.Width, g.Height, g.Stride, sampleFormat, quality);
            var rq = g.Request;
            sampler.Add(new Replay.ReplaySampler.Sample(rq.Ts, g.Monitor, rq.App, GuardrailPolicy.FilterTitle(rq.Title, s.Privacy), rq.Site,
                g.Width, g.Height, sampleBytes, ImageCodec.Mime(sampleFormat)), Math.Clamp(s.Replay.Seconds, 15, 600) + 10);
        }
        try { return ProcessKeyframe(g, s, store, sampleFormat, sampleBytes); }
        finally { if (sampleBytes is not null) Array.Clear(sampleBytes); }
    }

    private bool ProcessKeyframe(KeyframeGrab g, Settings s, IStore store, string sampleFormat, byte[]? sampleBytes)
    {
        if (!g.Request.Keyframe) return false;
        DedupFilter dedup;
        lock (_dedup)
        {
            if (!_dedup.TryGetValue(g.Monitor, out dedup!)) _dedup[g.Monitor] = dedup = new DedupFilter();
        }
        var sw = Stopwatch.StartNew();
        ulong hash = DHash.Compute(g.Span, g.Width, g.Height, g.Stride);
        var map = ChangeMap.Compute(g.Span, g.Width, g.Height, g.Stride);
        var (reason, change) = dedup.Decide(map, hash, g.Request, s.Capture.DhashThreshold, Math.Clamp(s.Capture.DirtyMinPercent, 0, 100));
        HashMsTotal += sw.Elapsed.TotalMilliseconds;
        LastChange = change;
        if (reason == KeepReason.Skip)
        {
            map.Clear();
            Interlocked.Increment(ref SkippedSimilar);
            return false;
        }
        // The next grabs compare against this frame even while its encode waits for budget.
        dedup.Accept(hash, g.Request.Context, map);

        // A replay sample (WebP or JPEG) is stored as is when the keyframe uses the same format: no second encode.
        if (sampleBytes is not null && ImageCodec.Resolve(s.Keyframes.Format) == sampleFormat)
        {
            Store(store, s, g, hash, change, reason, sampleBytes.ToArray(), sampleFormat, g.Width, g.Height);
            return false;
        }
        lock (_governor) _governor.Offer(new Pending(g, hash, change, reason));
        return true;
    }

    /// <summary>Governor callback: encode at the level, store, return the CPU ms the encode used on this thread.</summary>
    private unsafe double EncodeAndStore(Pending p, EncodeLevel level)
    {
        var s = _settings();
        var store = _store();
        if (store is null) return 0;
        var g = p.Grab;
        double cpu0 = Win32.CurrentThreadCpuMs();
        var sw = Stopwatch.StartNew();
        byte[] bytes;
        string format;
        int w, h;
        try { (bytes, format, w, h) = KeyframeEncoder.Encode(g.Pixels, g.Width, g.Height, g.Stride, s, level); }
        catch (Exception ex)
        {
            Interlocked.Increment(ref Failed);
            Log.Warn("keyframe encode", ex);
            return Math.Max(0, Win32.CurrentThreadCpuMs() - cpu0);
        }
        double cpu = Math.Max(0, Win32.CurrentThreadCpuMs() - cpu0);
        LastEncodeMs = sw.Elapsed.TotalMilliseconds;
        EncodeMsTotal += LastEncodeMs;
        EncodeCpuMsTotal += cpu;
        KeyframeEncoder.LogChange(ref _encoderName, KeyframeEncoder.Describe(format, level));
        try { Store(store, s, g, p.Hash, p.Change, p.Reason, bytes, format, w, h); }
        catch (Exception ex)
        {
            Interlocked.Increment(ref Failed);
            Log.Warn("keyframe store", ex);
        }
        return cpu;
    }

    private void Store(IStore store, Settings s, KeyframeGrab g, ulong hash, ChangeResult change, KeepReason reason, byte[] bytes, string format, int w, int h)
    {
        var req = g.Request;
        var title = GuardrailPolicy.FilterTitle(req.Title, s.Privacy);
        var cap = new NewScreenCapture(req.Ts, req.App, req.ExePath, title, g.Monitor, w, h, hash,
            bytes, ImageCodec.Mime(format), s.Keyframes.Enabled, req.Site,
            Change: change.Comparable ? Math.Round(change.Score, 2) : null, Switch: reason is KeepReason.First or KeepReason.Switch);
        string id;
        try { id = store.AddScreenCapture(cap); }
        finally { Array.Clear(bytes); }
        Interlocked.Increment(ref Kept);
        _stored(new CaptureMeta(id, req.Ts, CaptureKind.Screen, req.App, title, g.Monitor, 0, s.Keyframes.Enabled, false, req.Site));
    }

    public ValueTask DisposeAsync()
    {
        _channel.Writer.TryComplete();
        _cts.Cancel();
        _thread.Join(5000);
        while (_channel.Reader.TryRead(out var g)) g.Dispose();
        lock (_governor) _governor.Clear();
        return ValueTask.CompletedTask;
    }
}
