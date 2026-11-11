// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Threading.Channels;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Capture;

/// <summary>
/// Receives downscaled grabs from the capture threads: smart capture (change map and dHash against the last stored
/// keyframe, CapturePolicy), encode once (keyframes.format), IStore.AddScreenCapture. Runs on one background task; grabs are zeroed after use.
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
    private readonly KeyframeEncoder _encoder;
    private readonly Task _task;
    private readonly CancellationTokenSource _cts = new();

    // Measurements.
    public long Kept, SkippedSimilar, Dropped, Failed;
    public double EncodeMsTotal, HashMsTotal, GrabMsTotal;
    public double LastEncodeMs;
    public ChangeResult LastChange;

    /// <summary>Receives replay samples (ReplaySample requests). Null: samples are dropped.</summary>
    public Replay.ReplaySampler? Sampler { get; set; }

    /// <summary>Encoder of the last keyframe (log, status, bench).</summary>
    public string Encoder => _encoder.Active;
    public string? HardwareUnavailable => _encoder.HardwareUnavailable;

    /// <param name="devices">GPU devices for the AV1 hardware keyframe encoder; null or returning null: software only.</param>
    public KeyframePipeline(Func<IStore?> store, Func<Settings> settings, Action<CaptureMeta> stored, Func<GpuDeviceCache?>? devices = null)
    {
        _store = store;
        _settings = settings;
        _stored = stored;
        _encoder = new KeyframeEncoder(devices ?? (() => null));
        _task = Task.Run(() => RunAsync(_cts.Token));
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

    private async Task RunAsync(CancellationToken ct)
    {
        try
        {
            await foreach (var grab in _channel.Reader.ReadAllAsync(ct).ConfigureAwait(false))
            {
                using (grab)
                {
                    try { Process(grab); }
                    catch (Exception ex)
                    {
                        Interlocked.Increment(ref Failed);
                        Log.Warn("keyframe", ex);
                    }
                }
            }
        }
        catch (OperationCanceledException) { }
    }

    private unsafe void Process(KeyframeGrab g)
    {
        var s = _settings();
        var store = _store();
        if (store is null) return;
        string format = s.Keyframes.Format == "jpeg" ? "jpeg" : "webp";
        int quality = Math.Clamp(s.Keyframes.Quality, 30, 95);
        byte[]? sampleBytes = null;
        if (g.Request.ReplaySample && Sampler is { } sampler)
        {
            sampleBytes = ImageCodec.EncodeBgra(g.Pixels, g.Width, g.Height, g.Stride, format, quality);
            var rq = g.Request;
            sampler.Add(new Replay.ReplaySampler.Sample(rq.Ts, g.Monitor, rq.App, GuardrailPolicy.FilterTitle(rq.Title, s.Privacy), rq.Site,
                g.Width, g.Height, sampleBytes, ImageCodec.Mime(format)), Math.Clamp(s.Replay.Seconds, 15, 600) + 10);
        }
        try { ProcessKeyframe(g, s, store, format, quality, sampleBytes); }
        finally { if (sampleBytes is not null) Array.Clear(sampleBytes); }
    }

    private unsafe void ProcessKeyframe(KeyframeGrab g, Settings s, IStore store, string format, int quality, byte[]? encoded)
    {
        if (!g.Request.Keyframe) return;
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
            return;
        }
        sw.Restart();
        byte[] bytes;
        string kfFormat;
        try
        {
            // A replay sample (WebP or JPEG, which the worker decodes) is reused when the keyframe uses the same format.
            if (encoded is not null && ImageCodec.Resolve(s.Keyframes.Format) == format) (bytes, kfFormat) = (encoded.ToArray(), format);
            else (bytes, kfFormat) = _encoder.Encode(g.Pixels, g.Width, g.Height, g.Stride, s);
        }
        catch { map.Clear(); throw; }
        LastEncodeMs = sw.Elapsed.TotalMilliseconds;
        EncodeMsTotal += LastEncodeMs;
        var req = g.Request;
        var title = GuardrailPolicy.FilterTitle(req.Title, s.Privacy);
        var cap = new NewScreenCapture(req.Ts, req.App, req.ExePath, title, g.Monitor, g.Width, g.Height, hash,
            bytes, ImageCodec.Mime(format), s.Keyframes.Enabled, req.Site,
            Change: change.Comparable ? Math.Round(change.Score, 2) : null, Switch: reason is KeepReason.First or KeepReason.Switch);
        string id;
        try { id = store.AddScreenCapture(cap); }
        catch { map.Clear(); throw; }
        finally { Array.Clear(bytes); }
        dedup.Accept(hash, req.Context, map);
        Interlocked.Increment(ref Kept);
        _stored(new CaptureMeta(id, req.Ts, CaptureKind.Screen, req.App, title, g.Monitor, 0, s.Keyframes.Enabled, false, req.Site));
    }

    public async ValueTask DisposeAsync()
    {
        _channel.Writer.TryComplete();
        _cts.Cancel();
        try { await _task.ConfigureAwait(false); } catch { }
        while (_channel.Reader.TryRead(out var g)) g.Dispose();
        _encoder.Dispose();
    }
}
