// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Threading.Channels;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Privacy;
using BetterRecall.Agent.Scheduling;
using BetterRecall.Agent.Util;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Capture;

/// <summary>Reads the text of a BGRA keyframe (the worker's ocr op). Abstracted for tests.</summary>
internal interface IKeyframeReader
{
    Task<OcrResult> ReadAsync(ReadOnlyMemory<byte> bgra, int width, int height, int stride, bool entities, CancellationToken ct);
}

internal sealed class WorkerKeyframeReader(WorkerClient worker) : IKeyframeReader
{
    public Task<OcrResult> ReadAsync(ReadOnlyMemory<byte> bgra, int width, int height, int stride, bool entities, CancellationToken ct)
        => worker.OcrAsync(bgra, width, height, stride, entities, ct);
}

/// <summary>
/// Receives downscaled grabs from the capture threads: smart capture (change map and dHash against the last stored
/// keyframe, CapturePolicy), OCR (ocr.enabled), redaction (privacy.redact*: black boxes on the pixels, redacted words
/// left out of the text), the replay sample (WebP or JPEG, redacted), then the CPU budget (EncodeGovernor) decides when
/// and how each kept frame is encoded (KeyframeEncoder), then IStore.AddScreenCapture. Runs on its own below-normal
/// EcoQoS thread; grabs are zeroed after use.
/// Redaction happens before anything is encoded or stored. When OCR fails for a passing reason (worker restarting,
/// busy, timeout) and a text detector is on, the keyframe is dropped rather than stored unredacted; when the OCR model
/// is not installed, keyframes are stored without text (only password fields and the title are redacted) and a warning
/// is logged. OCR time is measured apart from encode time: the CPU budget covers the keyframe encode only.
/// </summary>
internal sealed class KeyframePipeline : IAsyncDisposable
{
    public static readonly TimeSpan OcrTimeout = TimeSpan.FromSeconds(30);

    private readonly Channel<KeyframeGrab> _channel = Channel.CreateBounded<KeyframeGrab>(new BoundedChannelOptions(6)
    {
        FullMode = BoundedChannelFullMode.DropWrite,
        SingleReader = true,
    });
    private readonly Func<IStore?> _store;
    private readonly Func<Settings> _settings;
    private readonly Action<CaptureMeta> _stored;
    private readonly IKeyframeReader? _reader;
    private readonly RedactionMasks? _masks;
    private readonly Dictionary<int, DedupFilter> _dedup = new();
    private readonly EncodeGovernor<Pending> _governor;
    private readonly Thread _thread;
    private readonly CancellationTokenSource _cts = new();
    private string _encoderName = "";
    private Task<bool>? _wait;
    private long _lastMissingLog = -1;
    private long _lastNerMissingLog = -1;

    // Measurements.
    public long Kept, SkippedSimilar, Dropped, Failed;
    public long OcrRead, OcrFailed, OcrUnavailable, RedactDropped, RedactedSpans;
    public double EncodeMsTotal, HashMsTotal, GrabMsTotal, EncodeCpuMsTotal;
    /// <summary>OCR wall time, and the CPU this thread spent on it (copy, IPC); the worker's own CPU is not included.</summary>
    public double OcrMsTotal, OcrCpuMsTotal;
    public double LastEncodeMs, LastOcrMs;
    public ChangeResult LastChange;

    /// <summary>Receives replay samples (ReplaySample requests), redacted like keyframes. Null: samples are dropped.</summary>
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

    /// <summary>A kept, redacted frame waiting for its encode. Owns the grab.</summary>
    private sealed class Pending(KeyframeGrab grab, ulong hash, ChangeResult change, KeepReason reason, string title, string? text) : IDisposable
    {
        public KeyframeGrab Grab { get; } = grab;
        public ulong Hash { get; } = hash;
        public ChangeResult Change { get; } = change;
        public KeepReason Reason { get; } = reason;
        public string Title { get; } = title;
        public string? Text { get; } = text;
        public void Dispose() => Grab.Dispose();
    }

    public KeyframePipeline(Func<IStore?> store, Func<Settings> settings, Action<CaptureMeta> stored,
        IKeyframeReader? reader = null, RedactionMasks? masks = null)
    {
        _store = store;
        _settings = settings;
        _stored = stored;
        _reader = reader;
        _masks = masks;
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
        _masks?.Clear();
    }

    private volatile bool _flush;

    /// <summary>Panic: the keyframe thread drops every queued grab and every frame waiting for CPU budget (their pixels
    /// leave RAM). A frame already being processed is refused by the store's purge fence.</summary>
    public void Flush() => _flush = true;

    private void FlushQueued()
    {
        _flush = false;
        int n = 0;
        while (_channel.Reader.TryRead(out var g)) { g.Dispose(); n++; }
        lock (_governor) _governor.Clear();
        Log.Info($"keyframes flushed ({n} queued grabs)");
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
            if (_flush) FlushQueued();
            if (_channel.Reader.TryRead(out var grab))
            {
                bool owned = false;
                try { owned = Process(grab, ct); }
                catch (OperationCanceledException) when (ct.IsCancellationRequested) { }
                catch (PurgedWindowException) { Log.Debug("keyframe inside a purged window dropped"); }
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
    private bool Process(KeyframeGrab g, CancellationToken ct)
    {
        var s = _settings();
        var store = _store();
        if (store is null) return false;
        var req = g.Request;
        bool sample = req.ReplaySample && Sampler is not null;
        bool keep = req.Keyframe;
        DedupFilter? dedup = null;
        ChangeMap? map = null;
        ulong hash = 0;
        var change = default(ChangeResult);
        var reason = KeepReason.Skip;
        var sw = Stopwatch.StartNew();
        if (keep)
        {
            lock (_dedup)
            {
                if (!_dedup.TryGetValue(g.Monitor, out dedup)) _dedup[g.Monitor] = dedup = new DedupFilter();
            }
            hash = DHash.Compute(g.Span, g.Width, g.Height, g.Stride);
            map = ChangeMap.Compute(g.Span, g.Width, g.Height, g.Stride);
            (reason, change) = dedup.Decide(map, hash, req, s.Capture.DhashThreshold, Math.Clamp(s.Capture.DirtyMinPercent, 0, 100));
            HashMsTotal += sw.Elapsed.TotalMilliseconds;
            LastChange = change;
            if (reason == KeepReason.Skip)
            {
                map.Clear();
                map = null;
                Interlocked.Increment(ref SkippedSimilar);
                keep = false;
            }
        }
        if (!keep && !sample) return false;

        // Redaction before anything is encoded: stored keyframes and replay samples get the same black boxes.
        var red = RedactionOptions.From(s.Privacy);
        bool hasBounds = req.Bounds.Width > 0 && req.Bounds.Height > 0;
        var secrets = red.Passwords && hasBounds && req.Secrets is { Length: > 0 } sr
            ? RedactionMasks.ToKeyframe(sr, req.Bounds, g.Width, g.Height) : [];

        OcrResult? ocr = null;
        if (s.Ocr.Enabled && _reader is not null)
        {
            double cpu0 = Win32.CurrentThreadCpuMs();
            sw.Restart();
            var (result, permanent, why) = Read(g, red.Entities, ct);
            LastOcrMs = sw.Elapsed.TotalMilliseconds;
            OcrMsTotal += LastOcrMs;
            OcrCpuMsTotal += Math.Max(0, Win32.CurrentThreadCpuMs() - cpu0);
            if (result is not null)
            {
                ocr = result;
                Interlocked.Increment(ref OcrRead);
                long now = Environment.TickCount64;
                if (result.EntitiesMissing && (_lastNerMissingLog < 0 || now - _lastNerMissingLog > 600_000))
                {
                    _lastNerMissingLog = now;
                    Log.Warn("ner model not installed: names and addresses are not redacted (pattern redaction still runs)");
                }
            }
            else if (permanent)
            {
                Interlocked.Increment(ref OcrUnavailable);
                long now = Environment.TickCount64;
                if (_lastMissingLog < 0 || now - _lastMissingLog > 600_000)
                {
                    _lastMissingLog = now;
                    Log.Warn($"ocr unavailable ({why}): keyframes stored without text; text redaction off until it is installed");
                }
            }
            else if (red.AnyText)
            {
                // Never store or sample a frame whose text could not be checked.
                map?.Clear();
                Interlocked.Increment(ref RedactDropped);
                Log.Debug($"keyframe dropped: ocr failed ({why})");
                return false;
            }
            else Interlocked.Increment(ref OcrFailed);
        }

        var title = GuardrailPolicy.FilterTitle(req.Title, s.Privacy);
        var r = Redactor.Apply(ocr?.Lines ?? [], secrets, ocr is null ? red with { Names = false, Addresses = false } : red, title);
        if (r.Boxes.Length > 0) FillBoxes(g, r.Boxes);
        if (hasBounds) _masks?.Set(g.Monitor, RedactionMasks.ToDesktop(r.Boxes, g.Width, g.Height, req.Bounds));
        Interlocked.Add(ref RedactedSpans, r.Spans);
        string? text = ocr is null ? null : r.Text;

        string sampleFormat = s.Keyframes.Format == "jpeg" ? "jpeg" : "webp";
        byte[]? sampleBytes = null;
        try
        {
            if (sample)
            {
                sampleBytes = Encode(g, sampleFormat, Math.Clamp(s.Keyframes.Quality, 30, 95));
                Sampler!.Add(new Replay.ReplaySampler.Sample(req.Ts, g.Monitor, req.App, r.Title, req.Site, g.Width, g.Height,
                    sampleBytes, ImageCodec.Mime(sampleFormat)), Math.Clamp(s.Replay.Seconds, 15, 600) + 10);
            }
            if (!keep) return false;
            // The next grabs compare against this frame even while its encode waits for budget.
            dedup!.Accept(hash, req.Context, map!);

            // A replay sample (WebP or JPEG) is stored as is when the keyframe uses the same format: no second encode.
            if (sampleBytes is not null && ImageCodec.Resolve(s.Keyframes.Format) == sampleFormat)
            {
                Store(store, s, g, hash, change, reason, r.Title, text, sampleBytes.ToArray(), sampleFormat, g.Width, g.Height);
                return false;
            }
            lock (_governor) _governor.Offer(new Pending(g, hash, change, reason, r.Title, text));
            return true;
        }
        finally { if (sampleBytes is not null) Array.Clear(sampleBytes); }
    }

    /// <summary>OCR result, or why there is none: permanent (model or Windows OCR language not installed) or passing.
    /// Blocks this thread: the pipeline thread has no synchronization context.</summary>
    private (OcrResult? Result, bool Permanent, string Why) Read(KeyframeGrab g, bool entities, CancellationToken ct)
    {
        var copy = CopyPixels(g);
        try
        {
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
            timeout.CancelAfter(OcrTimeout);
            var r = _reader!.ReadAsync(copy, g.Width, g.Height, g.Stride, entities, timeout.Token).GetAwaiter().GetResult();
            return (r, false, "");
        }
        catch (WorkerException ex) when (ex.Message.StartsWith("Model not installed", StringComparison.Ordinal))
        {
            return (null, true, ex.Message);
        }
        catch (WorkerException ex) { return (null, false, ex.Message); }
        catch (OperationCanceledException) when (!ct.IsCancellationRequested) { return (null, false, "timeout"); }
        finally { Array.Clear(copy); }
    }

    private static byte[] CopyPixels(KeyframeGrab g)
    {
        var copy = new byte[g.Stride * g.Height];
        g.Span.CopyTo(copy);
        return copy;
    }

    private static unsafe void FillBoxes(KeyframeGrab g, Box[] boxes) => RedactionMasks.Fill(g.Pixels, g.Width, g.Height, g.Stride, boxes);

    private static unsafe byte[] Encode(KeyframeGrab g, string format, int quality)
        => ImageCodec.EncodeBgra(g.Pixels, g.Width, g.Height, g.Stride, format, quality);

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
        try { Store(store, s, g, p.Hash, p.Change, p.Reason, p.Title, p.Text, bytes, format, w, h); }
        catch (PurgedWindowException) { Log.Debug("keyframe inside a purged window dropped"); }
        catch (Exception ex)
        {
            Interlocked.Increment(ref Failed);
            Log.Warn("keyframe store", ex);
        }
        return cpu;
    }

    private void Store(IStore store, Settings s, KeyframeGrab g, ulong hash, ChangeResult change, KeepReason reason, string title, string? text,
        byte[] bytes, string format, int w, int h)
    {
        var req = g.Request;
        var cap = new NewScreenCapture(req.Ts, req.App, req.ExePath, title, g.Monitor, w, h, hash,
            bytes, ImageCodec.Mime(format), s.Keyframes.Enabled, req.Site,
            Change: change.Comparable ? Math.Round(change.Score, 2) : null, Switch: reason is KeepReason.First or KeepReason.Switch, Text: text);
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
