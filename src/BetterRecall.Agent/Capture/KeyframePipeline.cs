// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Threading.Channels;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Privacy;
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
/// Receives downscaled grabs from the capture threads: dHash de-dup, OCR (ocr.enabled), redaction (privacy.redact*:
/// black boxes on the pixels, redacted words left out of the text), encode once (WebP or JPEG), IStore.AddScreenCapture.
/// Runs on one background task; grabs are zeroed after use.
/// Redaction happens before anything is stored. When OCR fails for a passing reason (worker restarting, busy, timeout)
/// and a text detector is on, the keyframe is dropped rather than stored unredacted; when the OCR model is not
/// installed, keyframes are stored without text (only password fields and the title are redacted) and a warning is
/// logged.
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
    private readonly Task _task;
    private readonly CancellationTokenSource _cts = new();
    private long _lastMissingLog = -1;

    // Measurements.
    public long Kept, SkippedSimilar, Dropped, Failed;
    public long OcrRead, OcrFailed, OcrUnavailable, RedactDropped, RedactedSpans;
    public double EncodeMsTotal, HashMsTotal, GrabMsTotal, OcrMsTotal;
    public double LastEncodeMs, LastOcrMs;

    public KeyframePipeline(Func<IStore?> store, Func<Settings> settings, Action<CaptureMeta> stored,
        IKeyframeReader? reader = null, RedactionMasks? masks = null)
    {
        _store = store;
        _settings = settings;
        _stored = stored;
        _reader = reader;
        _masks = masks;
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
        _masks?.Clear();
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
                    try { await ProcessAsync(grab, ct).ConfigureAwait(false); }
                    catch (OperationCanceledException) when (ct.IsCancellationRequested) { throw; }
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

    private async Task ProcessAsync(KeyframeGrab g, CancellationToken ct)
    {
        var s = _settings();
        var store = _store();
        if (store is null) return;
        DedupFilter dedup;
        lock (_dedup)
        {
            if (!_dedup.TryGetValue(g.Monitor, out dedup!)) _dedup[g.Monitor] = dedup = new DedupFilter();
        }
        var sw = Stopwatch.StartNew();
        ulong hash = DHash.Compute(g.Span, g.Width, g.Height, g.Stride);
        HashMsTotal += sw.Elapsed.TotalMilliseconds;
        if (!g.Request.Force && dedup.HashCheck(hash, s.Capture.DhashThreshold, g.Request.Context) == DedupVerdict.SkipSimilar)
        {
            Interlocked.Increment(ref SkippedSimilar);
            return;
        }

        var red = RedactionOptions.From(s.Privacy);
        var req = g.Request;
        bool hasBounds = req.Bounds.Width > 0 && req.Bounds.Height > 0;
        var secrets = red.Passwords && hasBounds && req.Secrets is { Length: > 0 } sr
            ? RedactionMasks.ToKeyframe(sr, req.Bounds, g.Width, g.Height) : [];

        OcrResult? ocr = null;
        if (s.Ocr.Enabled && _reader is not null)
        {
            sw.Restart();
            var (result, permanent, why) = await ReadAsync(g, red.Entities, ct).ConfigureAwait(false);
            LastOcrMs = sw.Elapsed.TotalMilliseconds;
            OcrMsTotal += LastOcrMs;
            if (result is not null) { ocr = result; Interlocked.Increment(ref OcrRead); }
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
                // Redaction before saving: never store a frame whose text could not be checked.
                Interlocked.Increment(ref RedactDropped);
                Log.Debug($"keyframe dropped: ocr failed ({why})");
                return;
            }
            else Interlocked.Increment(ref OcrFailed);
        }

        var title = GuardrailPolicy.FilterTitle(req.Title, s.Privacy);
        var r = Redactor.Apply(ocr?.Lines ?? [], secrets, ocr is null ? red with { Names = false, Addresses = false } : red, title);
        if (r.Boxes.Length > 0) FillBoxes(g, r.Boxes);
        if (hasBounds) _masks?.Set(g.Monitor, RedactionMasks.ToDesktop(r.Boxes, g.Width, g.Height, req.Bounds));
        Interlocked.Add(ref RedactedSpans, r.Spans);

        sw.Restart();
        string format = s.Keyframes.Format == "jpeg" ? "jpeg" : "webp";
        byte[] bytes = Encode(g, format, Math.Clamp(s.Keyframes.Quality, 30, 95));
        LastEncodeMs = sw.Elapsed.TotalMilliseconds;
        EncodeMsTotal += LastEncodeMs;
        var cap = new NewScreenCapture(req.Ts, req.App, req.ExePath, r.Title, g.Monitor, g.Width, g.Height, hash,
            bytes, ImageCodec.Mime(format), s.Keyframes.Enabled, req.Site, ocr is null ? null : r.Text);
        string id;
        try { id = store.AddScreenCapture(cap); }
        finally { Array.Clear(bytes); }
        dedup.Accept(hash, req.Context);
        Interlocked.Increment(ref Kept);
        _stored(new CaptureMeta(id, req.Ts, CaptureKind.Screen, req.App, r.Title, g.Monitor, 0, s.Keyframes.Enabled, false, req.Site));
    }

    /// <summary>OCR result, or why there is none: permanent (model or Windows OCR language not installed) or passing.</summary>
    private async Task<(OcrResult? Result, bool Permanent, string Why)> ReadAsync(KeyframeGrab g, bool entities, CancellationToken ct)
    {
        var copy = CopyPixels(g);
        try
        {
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
            timeout.CancelAfter(OcrTimeout);
            var r = await _reader!.ReadAsync(copy, g.Width, g.Height, g.Stride, entities, timeout.Token).ConfigureAwait(false);
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

    private static unsafe byte[] CopyPixels(KeyframeGrab g)
    {
        var copy = new byte[g.Stride * g.Height];
        g.Span.CopyTo(copy);
        return copy;
    }

    private static unsafe void FillBoxes(KeyframeGrab g, Box[] boxes) => RedactionMasks.Fill(g.Pixels, g.Width, g.Height, g.Stride, boxes);

    private static unsafe byte[] Encode(KeyframeGrab g, string format, int quality)
        => ImageCodec.EncodeBgra(g.Pixels, g.Width, g.Height, g.Stride, format, quality);

    public async ValueTask DisposeAsync()
    {
        _channel.Writer.TryComplete();
        _cts.Cancel();
        try { await _task.ConfigureAwait(false); } catch { }
        while (_channel.Reader.TryRead(out var g)) g.Dispose();
    }
}
